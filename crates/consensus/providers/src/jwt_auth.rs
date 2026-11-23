//! Tower layer that attaches a signed Engine API JWT `Authorization: Bearer` header to Alloy
//! JSON-RPC [`RequestPacket`]s.

use std::{
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_rpc_types_engine::{Claims, JwtSecret};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut, TransportResult};
use alloy_transport_http::{Http, reqwest};
use http::{HeaderValue, header::AUTHORIZATION};
use tower::{Layer, Service};
use url::Url;

/// Tower layer that injects a fresh Engine API JWT `Authorization: Bearer` header into outbound
/// JSON-RPC [`RequestPacket`]s.
///
/// Works directly with `alloy_transport_http::Http<reqwest::Client>` (rustls), avoiding
/// `alloy-transport-http`'s `hyper` / `jwt-auth` features which pull in `hyper-tls` and `OpenSSL`.
#[derive(Clone, Copy, Debug)]
pub struct JwtAuthLayer {
    secret: JwtSecret,
}

impl JwtAuthLayer {
    /// Creates a new [`JwtAuthLayer`] signing requests with `secret`.
    pub const fn new(secret: JwtSecret) -> Self {
        Self { secret }
    }

    /// Builds a direct [`Http`] transport for JWT-authenticated Engine/L2 RPC calls.
    ///
    /// Disables system proxy lookup (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`) and HTTP redirects so
    /// authenticated RPC traffic always goes directly to the configured endpoint and never leaks
    /// bearer tokens to a proxy or redirect target.
    pub fn build_http_transport(url: Url) -> TransportResult<Http<reqwest::Client>> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(TransportErrorKind::custom)?;
        Ok(Http::with_client(client, url))
    }
}

impl<S> Layer<S> for JwtAuthLayer {
    type Service = JwtAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        JwtAuthService { inner, secret: self.secret }
    }
}

/// Tower service produced by [`JwtAuthLayer`].
#[derive(Clone, Debug)]
pub struct JwtAuthService<S> {
    inner: S,
    secret: JwtSecret,
}

impl<S> Service<RequestPacket> for JwtAuthService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: RequestPacket) -> Self::Future {
        let iat = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let token = match self.secret.encode(&Claims { iat, exp: None }) {
            Ok(token) => token,
            Err(err) => {
                let err = TransportErrorKind::custom(err);
                return Box::pin(async move { Err(err) });
            }
        };
        let header = match HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(header) => header,
            Err(err) => {
                let err = TransportErrorKind::custom(err);
                return Box::pin(async move { Err(err) });
            }
        };

        match &mut req {
            RequestPacket::Single(single) => {
                single.headers_mut().insert(AUTHORIZATION, header);
            }
            RequestPacket::Batch(batch) => {
                if let Some(first) = batch.first_mut() {
                    first.headers_mut().insert(AUTHORIZATION, header);
                }
            }
        }

        Box::pin(self.inner.call(req))
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use alloy_json_rpc::{Id, Request, RequestPacket, ResponsePacket, SerializedRequest};
    use alloy_rpc_client::ClientBuilder;
    use alloy_rpc_types_engine::JwtSecret;
    use alloy_transport::TransportError;
    use http::header::AUTHORIZATION;
    use httpmock::prelude::*;
    use tower::{Layer, Service, service_fn};
    use url::Url;

    use super::JwtAuthLayer;

    fn dummy_request() -> SerializedRequest {
        Request::new("engine_exchangeCapabilities", Id::Number(1), ()).serialize().unwrap()
    }

    fn dummy_response() -> ResponsePacket {
        serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":[]}"#).unwrap()
    }

    #[tokio::test]
    async fn injects_valid_bearer_jwt_on_single_and_batch_packets() {
        let secret = JwtSecret::random();
        let layer = JwtAuthLayer::new(secret);

        for packet in [
            RequestPacket::Single(dummy_request()),
            RequestPacket::Batch(vec![dummy_request(), dummy_request()]),
        ] {
            let mut svc = layer.layer(service_fn(move |req: RequestPacket| {
                let headers = req.headers();
                let auth = headers
                    .get(AUTHORIZATION)
                    .expect("Authorization header should be set")
                    .to_str()
                    .expect("header should be valid UTF-8");
                let token = auth.strip_prefix("Bearer ").expect("header should have Bearer prefix");
                secret.validate(token).expect("JWT token should validate");
                ready(Ok::<_, TransportError>(dummy_response()))
            }));
            svc.call(packet).await.unwrap();
        }
    }

    #[tokio::test]
    async fn http_transport_sends_valid_jwt_and_refuses_redirects() {
        let server = MockServer::start();
        let secret = JwtSecret::random();
        let expected_secret = secret;

        let ok_mock = server.mock(move |when, then| {
            when.method(POST).path("/ok").is_true(move |req| {
                req.headers()
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .is_some_and(|token| expected_secret.validate(token).is_ok())
            });
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#);
        });

        let redirect_mock = server.mock(|when, then| {
            when.method(POST).path("/redirect");
            then.status(302).header("location", "/ok");
        });

        let ok_url = Url::parse(&server.url("/ok")).unwrap();
        let ok_client = ClientBuilder::default()
            .layer(JwtAuthLayer::new(secret))
            .transport(JwtAuthLayer::build_http_transport(ok_url).unwrap(), false);
        let result: String = ok_client.request_noparams("eth_chainId").await.unwrap();
        assert_eq!(result, "0x1");
        ok_mock.assert_calls(1);

        let redirect_url = Url::parse(&server.url("/redirect")).unwrap();
        let redirect_client = ClientBuilder::default()
            .layer(JwtAuthLayer::new(secret))
            .transport(JwtAuthLayer::build_http_transport(redirect_url).unwrap(), false);
        let err = redirect_client.request_noparams::<String>("eth_chainId").await.unwrap_err();
        assert!(
            err.to_string().contains("302"),
            "expected 302 error without following redirect: {err}"
        );
        redirect_mock.assert_calls(1);
        ok_mock.assert_calls(1);
    }

    #[test]
    fn http_transport_ignores_http_proxy_env() {
        // Verify in an isolated child process with a bogus HTTP_PROXY so parallel tests are never
        // affected by process-global environment mutation.
        if std::env::var_os("BASE_TEST_JWT_NO_PROXY_CHILD").is_some() {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let server = MockServer::start();
                let secret = JwtSecret::random();
                let mock = server.mock(|when, then| {
                    when.method(POST).path("/");
                    then.status(200)
                        .header("content-type", "application/json")
                        .body(r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#);
                });
                let url = Url::parse(&server.url("/")).unwrap();
                let client = ClientBuilder::default()
                    .layer(JwtAuthLayer::new(secret))
                    .transport(JwtAuthLayer::build_http_transport(url).unwrap(), false);
                let result: String = client.request_noparams("eth_chainId").await.unwrap();
                assert_eq!(result, "0x1");
                mock.assert_calls(1);
            });
            return;
        }

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "jwt_auth::tests::http_transport_ignores_http_proxy_env"])
            .env("BASE_TEST_JWT_NO_PROXY_CHILD", "1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("http_proxy", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("NO_PROXY", "")
            .status()
            .unwrap();
        assert!(status.success(), "child test with bogus HTTP_PROXY failed");
    }
}
