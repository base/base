//! Tower layer that attaches a signed Engine API JWT `Authorization: Bearer` header to Alloy
//! JSON-RPC [`RequestPacket`]s.

use std::{
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_rpc_types_engine::{Claims, JwtSecret};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use http::{HeaderValue, header::AUTHORIZATION};
use tower::{Layer, Service};

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
    use alloy_rpc_types_engine::JwtSecret;
    use alloy_transport::TransportError;
    use http::header::AUTHORIZATION;
    use tower::{Layer, Service, service_fn};

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
}
