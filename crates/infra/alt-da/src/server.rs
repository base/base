use std::net::SocketAddr;

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{
    MAX_OBJECT_BYTES, StoreOpener,
    commitment::{decode_hex_commitment, generate_generic_commitment},
    error::{Error, InternalError, StoreError},
    store::DynStore,
};

/// Alt-DA HTTP server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP listen port.
    pub port: u16,
    /// Backing store URL (`s3://…` or `file://…`).
    pub da_url: String,
}

/// Alt-DA HTTP server.
pub struct Server {
    store: DynStore,
    addr: SocketAddr,
}

impl Server {
    /// Create a server bound to `0.0.0.0:{port}`.
    pub async fn new(config: Config) -> Result<Self, Error> {
        let store = StoreOpener::open(&config.da_url).await?;
        Ok(Self { store, addr: SocketAddr::from(([0, 0, 0, 0], config.port)) })
    }

    /// Serve until `shutdown` is cancelled (SIGINT/SIGTERM in the binary).
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), Error> {
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .map_err(|err| InternalError::Http(err.to_string()))?;
        info!(%self.addr, "alt-da server listening");
        let app = router(self.store);
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .map_err(|err| Error::Internal(InternalError::Http(err.to_string())))?;
        Ok(())
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("addr", &self.addr).finish_non_exhaustive()
    }
}

/// Build the axum router (used by tests and `run`).
pub(crate) fn router(store: DynStore) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/get/{commitment}", get(get_commitment))
        .route("/put", post(put_generate))
        .layer(DefaultBodyLimit::max(MAX_OBJECT_BYTES))
        .with_state(store)
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn get_commitment(
    State(store): State<DynStore>,
    Path(commitment): Path<String>,
) -> Result<Response, ApiError> {
    let key = decode_hex_commitment(&commitment)?;
    let value = store.get(&key).await?;
    Ok((StatusCode::OK, value).into_response())
}

async fn put_generate(State(store): State<DynStore>, body: Bytes) -> Result<Response, ApiError> {
    if body.is_empty() {
        return Err(ApiError(Error::BadRequest("empty request body".into())));
    }
    // Commitment is random; on store failure the batcher must retry POST /put (new commitment).
    let key = generate_generic_commitment();
    store.put(&key, &body).await?;
    Ok((StatusCode::OK, key).into_response())
}

struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        Self(err)
    }
}

impl From<crate::error::StoreError> for ApiError {
    fn from(err: crate::error::StoreError) -> Self {
        Self(Error::from(err))
    }
}

fn client_error_message(err: &Error) -> String {
    match err {
        Error::BadRequest(msg) => msg.clone(),
        Error::NotFound => "commitment not found".to_string(),
        Error::Config(err) => err.to_string(),
        Error::Store(StoreError::ObjectTooLarge { size, max }) => {
            format!("object too large: {size} bytes (max {max})")
        }
        Error::Store(_) | Error::Internal(_) => "internal server error".to_string(),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Config(err) => {
                error!(%err, "alt-da config error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Error::Store(StoreError::ObjectTooLarge { .. }) => StatusCode::PAYLOAD_TOO_LARGE,
            Error::Store(err) => {
                error!(%err, "alt-da store request failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Error::Internal(err) => {
                error!(%err, "alt-da internal error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, client_error_message(&self.0)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_health() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().display());
        let store = StoreOpener::open(&url).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(store);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp =
            reqwest::Client::new().get(format!("http://{addr}/health")).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_get_missing_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().display());
        let store = StoreOpener::open(&url).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(store);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp =
            reqwest::Client::new().get(format!("http://{addr}/get/0x0101")).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn http_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().display());
        let store = StoreOpener::open(&url).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(store);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let body: &[u8] = b"hello-batch";
        let put_resp = client.post(format!("http://{addr}/put")).body(body).send().await.unwrap();
        assert_eq!(put_resp.status(), StatusCode::OK);
        let commitment = put_resp.bytes().await.unwrap();
        let hex = format!("0x{}", hex::encode(&commitment));
        let get_resp = client.get(format!("http://{addr}/get/{hex}")).send().await.unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK);
        assert_eq!(get_resp.bytes().await.unwrap().as_ref(), body);
    }
}
