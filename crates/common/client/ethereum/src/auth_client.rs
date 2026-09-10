use std::{
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http::{HeaderValue, header::AUTHORIZATION};
use tower::{Layer, Service};

use base_common_types_payload::{Claims, JwtSecret};

/// A layer that adds a new JWT token to every request using `AuthClientService`.
#[derive(Debug)]
pub struct AuthClientLayer {
    secret: JwtSecret,
}

impl AuthClientLayer {
    /// Create a new `AuthClientLayer` with the given `secret`.
    pub const fn new(secret: JwtSecret) -> Self {
        Self { secret }
    }
}

impl<S> Layer<S> for AuthClientLayer {
    type Service = AuthClientService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthClientService::new(self.secret, inner)
    }
}

/// Automatically authenticates every client request with the given `secret`.
#[derive(Debug, Clone)]
pub struct AuthClientService<S> {
    secret: JwtSecret,
    inner: S,
}

impl<S> AuthClientService<S> {
    pub const fn new(secret: JwtSecret, inner: S) -> Self {
        Self { secret, inner }
    }
}

impl<S, B> Service<http::Request<B>> for AuthClientService<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        request.headers_mut().insert(AUTHORIZATION, AuthClientLayer::bearer_header(&self.secret));
        self.inner.call(request)
    }
}

impl AuthClientLayer {
    /// Helper function to convert a secret into a Bearer auth header value with claims according to
    /// <https://github.com/ethereum/execution-apis/blob/main/src/engine/authentication.md#jwt-claims>.
    /// The token is valid for 60 seconds.
    pub fn bearer_header(secret: &JwtSecret) -> HeaderValue {
        format!(
            "Bearer {}",
            secret
                .encode(&Claims {
                    iat: (SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
                        + Duration::from_secs(60))
                    .as_secs(),
                    exp: None,
                })
                .unwrap()
        )
        .parse()
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use base_common_types_payload::JwtSecret;
    use http::{Request, header::AUTHORIZATION};
    use tower::{Layer, ServiceExt, service_fn};

    use super::AuthClientLayer;

    #[tokio::test]
    async fn authenticates_and_preserves_the_request() {
        let secret = JwtSecret::random();
        let inner = service_fn(move |request: Request<String>| async move {
            let header = request.headers()[AUTHORIZATION].to_str().unwrap();
            secret.validate(header.strip_prefix("Bearer ").unwrap()).unwrap();
            assert_eq!(request.uri(), "/rpc");
            assert_eq!(request.headers()["x-request-id"], "test-request");
            Ok::<_, Infallible>(request.into_body())
        });
        let client = AuthClientLayer::new(secret).layer(inner);
        let request = Request::post("/rpc")
            .header(AUTHORIZATION, "stale credential")
            .header("x-request-id", "test-request")
            .body("rpc request".to_owned())
            .unwrap();
        assert_eq!(client.oneshot(request).await.unwrap(), "rpc request");
    }
}
