//! HTTP client for the alt-DA `Put`/`Get` API.

use std::time::Duration;

use url::Url;

use crate::{
    commitment::{GENERIC_COMMITMENT_LEN, GenericCommitment, validate_generic_commitment},
    error::ClientError,
};

/// Max PUT response bytes read before rejecting the body.
const MAX_PUT_RESPONSE_BYTES: usize = GENERIC_COMMITMENT_LEN + 1;

/// Alt-DA HTTP client used by the batcher to upload batch bytes.
#[derive(Debug, Clone)]
pub struct Client {
    put_url: Url,
    http: reqwest::Client,
}

impl Client {
    /// Build a client targeting `server` (e.g. `http://base-da-server:2583`).
    pub fn new(server: Url) -> Result<Self, ClientError> {
        let mut put_url = server;
        put_url.set_path("put");
        put_url.set_query(None);
        put_url.set_fragment(None);
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
        Ok(Self { put_url, http })
    }

    /// Upload batch bytes; returns the server-generated generic commitment.
    pub async fn put(&self, body: &[u8]) -> Result<GenericCommitment, ClientError> {
        if body.is_empty() {
            return Err(ClientError::EmptyBody);
        }
        if body.len() > crate::MAX_OBJECT_BYTES {
            return Err(ClientError::BodyTooLarge {
                size: body.len(),
                max: crate::MAX_OBJECT_BYTES,
            });
        }

        let resp = self.http.post(self.put_url.clone()).body(body.to_vec()).send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::UnexpectedStatus { status: status.as_u16() });
        }

        if let Some(len) = resp.content_length()
            && len as usize > MAX_PUT_RESPONSE_BYTES
        {
            return Err(ClientError::InvalidCommitmentLen { len: len as usize });
        }

        let mut bytes = Vec::with_capacity(GENERIC_COMMITMENT_LEN);
        let mut response = resp;
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > MAX_PUT_RESPONSE_BYTES {
                return Err(ClientError::InvalidCommitmentLen { len: bytes.len() + chunk.len() });
            }
            bytes.extend_from_slice(&chunk);
        }

        // Validate the server response at the boundary so downstream code can treat a
        // GenericCommitment as always-valid (lets encode_commitment_tx_data be infallible).
        validate_generic_commitment(&bytes).map_err(|_| ClientError::InvalidCommitment)?;
        let commitment: GenericCommitment = bytes
            .as_slice()
            .try_into()
            .map_err(|_| ClientError::InvalidCommitmentLen { len: bytes.len() })?;
        Ok(commitment)
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use crate::{Client, server::router, store::StoreOpener};

    async fn spawn_test_server() -> (Url, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store_url = format!("file://{}", dir.path().display());
        let store = StoreOpener::open(&store_url).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(store);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{addr}")).unwrap(), dir)
    }

    #[tokio::test]
    async fn put_returns_generic_commitment() {
        let (url, _dir) = spawn_test_server().await;
        let client = Client::new(url).unwrap();
        let commitment = client.put(b"hello-batch").await.unwrap();
        assert_eq!(commitment.len(), 34);
        assert_eq!(commitment[0], 0x01);
        assert_eq!(commitment[1], 0xff);
    }

    #[tokio::test]
    async fn rejects_empty_body() {
        let (url, _dir) = spawn_test_server().await;
        let client = Client::new(url).unwrap();
        let err = client.put(&[]).await.unwrap_err();
        assert!(matches!(err, crate::ClientError::EmptyBody));
    }
}
