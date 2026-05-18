//! WebSocket transport with per-connect JWT authentication.
//!
//! [`JwtWsConnect`] implements [`PubSubConnect`] by minting a fresh JWT on
//! every connect (and reconnect) attempt, ensuring the `iat` claim is always
//! within the ±60-second window enforced by execution-layer clients such as
//! Reth and Geth.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_pubsub::{ConnectionHandle, PubSubConnect};
use alloy_rpc_types_engine::{Claims, JwtSecret};
use alloy_transport::{Authorization, TransportResult};
use alloy_transport_ws::WsConnect;
use url::{Host, Url};

/// A [`PubSubConnect`] implementation that mints a fresh JWT on every connect
/// attempt.
///
/// Unlike [`WsConnect::with_auth`], which encodes a single token into the
/// upgrade headers and replays it verbatim on every internal reconnect,
/// [`JwtWsConnect`] calls [`JwtSecret::encode`] each time
/// [`PubSubConnect::connect`] is invoked. This ensures the `iat` claim is
/// always within the ±60-second window enforced by Reth and Geth, even after
/// a long-lived connection drops and reconnects.
#[derive(Clone, Debug)]
pub struct JwtWsConnect {
    /// The WebSocket endpoint URL.
    addr: Url,
    /// The JWT secret used to mint bearer tokens.
    jwt: JwtSecret,
}

impl JwtWsConnect {
    /// Creates a new [`JwtWsConnect`] for the given endpoint and secret.
    pub const fn new(addr: Url, jwt: JwtSecret) -> Self {
        Self { addr, jwt }
    }

    /// Generates a fresh [`Authorization`] bearer token stamped with the
    /// current Unix timestamp as the `iat` claim.
    ///
    /// The `exp` field is `None`, matching the authentication model used by
    /// the HTTP [`AuthLayer`].
    ///
    /// [`AuthLayer`]: alloy_transport_http::AuthLayer
    pub fn mint_bearer(&self) -> Authorization {
        let iat = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let token =
            self.jwt.encode(&Claims { iat, exp: None }).expect("jwt encoding is infallible");
        Authorization::bearer(token)
    }
}

impl PubSubConnect for JwtWsConnect {
    fn is_local(&self) -> bool {
        match self.addr.host() {
            Some(Host::Domain(d)) => d == "localhost",
            Some(Host::Ipv4(addr)) => addr == Ipv4Addr::LOCALHOST || addr == Ipv4Addr::UNSPECIFIED,
            Some(Host::Ipv6(addr)) => addr == Ipv6Addr::LOCALHOST || addr == Ipv6Addr::UNSPECIFIED,
            None => false,
        }
    }

    async fn connect(&self) -> TransportResult<ConnectionHandle> {
        WsConnect::new(self.addr.as_str()).with_auth(self.mint_bearer()).connect().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_rpc_types_engine::JwtSecret;
    use rstest::*;
    use tokio::{net::TcpListener, sync::oneshot};
    use tokio_tungstenite::{
        accept_async, accept_hdr_async, tungstenite::handshake::server::Request as WsRequest,
    };

    use super::*;

    async fn free_port_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// Accepts one WebSocket upgrade without inspecting headers.
    async fn accept_one(listener: TcpListener) {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = accept_async(stream).await;
        }
    }

    /// Accepts one WebSocket upgrade and returns the raw `Authorization` header value.
    async fn capture_auth_header(listener: TcpListener) -> String {
        capture_auth_header_from_listener(&listener).await
    }

    /// Like [`capture_auth_header`] but borrows the listener, allowing it to
    /// be reused for subsequent accepts.
    async fn capture_auth_header_from_listener(listener: &TcpListener) -> String {
        let (stream, _) = listener.accept().await.unwrap();
        let (tx, rx) = oneshot::channel::<String>();
        let _ = accept_hdr_async(stream, move |req: &WsRequest, resp| {
            let auth = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let _ = tx.send(auth);
            Ok(resp)
        })
        .await;
        rx.await.unwrap_or_default()
    }

    #[fixture]
    fn jwt_secret() -> JwtSecret {
        JwtSecret::random()
    }

    // ── unit tests (no network) ──────────────────────────────────────────────

    /// `is_local` returns `true` for loopback/unspecified addresses and
    /// `false` for remote hosts.
    #[rstest]
    #[case("ws://localhost:8551", true)]
    #[case("ws://127.0.0.1:8551", true)]
    #[case("ws://0.0.0.0:8551", true)]
    #[case("ws://[::1]:8551", true)]
    #[case("ws://[::]:8551", true)]
    #[case("ws://192.168.1.1:8551", false)]
    #[case("ws://example.com:8551", false)]
    fn is_local_detects_loopback(jwt_secret: JwtSecret, #[case] url: &str, #[case] expected: bool) {
        let conn = JwtWsConnect::new(url.parse().unwrap(), jwt_secret);
        assert_eq!(conn.is_local(), expected);
    }

    /// `mint_bearer` produces a valid JWT whose `iat` passes engine-api
    /// validation (i.e. lies within ±60 seconds of the current time).
    #[rstest]
    fn mint_bearer_iat_is_current(jwt_secret: JwtSecret) {
        let conn = JwtWsConnect::new("ws://127.0.0.1:8551".parse().unwrap(), jwt_secret);
        let Authorization::Bearer(token) = conn.mint_bearer() else {
            panic!("expected Bearer variant");
        };
        jwt_secret.validate(&token).expect("freshly minted token must pass engine-api validation");
    }

    /// Two `mint_bearer` calls separated by one second produce different tokens,
    /// confirming the `iat` is regenerated on every invocation rather than
    /// cached.
    #[rstest]
    #[tokio::test]
    async fn mint_bearer_rotates_after_one_second(jwt_secret: JwtSecret) {
        let conn = JwtWsConnect::new("ws://127.0.0.1:8551".parse().unwrap(), jwt_secret);
        let Authorization::Bearer(first) = conn.mint_bearer() else { unreachable!() };
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Authorization::Bearer(second) = conn.mint_bearer() else { unreachable!() };
        assert_ne!(first, second, "tokens must differ after 1 s");
        jwt_secret.validate(&first).expect("first token must be valid");
        jwt_secret.validate(&second).expect("second token must be valid");
    }

    // ── integration tests (require a local TCP listener) ─────────────────────

    /// `connect` completes the WebSocket handshake successfully.
    #[rstest]
    #[tokio::test]
    async fn connect_succeeds(jwt_secret: JwtSecret) {
        let (listener, port) = free_port_listener().await;
        tokio::spawn(accept_one(listener));
        JwtWsConnect::new(format!("ws://127.0.0.1:{port}").parse().unwrap(), jwt_secret)
            .connect()
            .await
            .expect("connect must succeed");
    }

    /// `connect` includes a valid JWT in the HTTP upgrade `Authorization` header.
    #[rstest]
    #[tokio::test]
    async fn connect_sends_valid_jwt(jwt_secret: JwtSecret) {
        let (listener, port) = free_port_listener().await;
        let capture = tokio::spawn(capture_auth_header(listener));
        JwtWsConnect::new(format!("ws://127.0.0.1:{port}").parse().unwrap(), jwt_secret)
            .connect()
            .await
            .expect("connect must succeed");
        let auth = capture.await.unwrap();
        let token = auth.strip_prefix("Bearer ").expect("header must use Bearer scheme");
        jwt_secret.validate(token).expect("token must pass engine-api validation");
    }

    /// Sequential `connect` calls on the *same* [`JwtWsConnect`] instance
    /// produce distinct JWTs, confirming the token is minted fresh on each
    /// attempt rather than replayed.
    ///
    /// This is the core regression test for the stale-`iat` reconnect bug: if
    /// `iat` were cached at construction time (as with [`WsConnect::with_auth`]),
    /// Reth and Geth would reject the handshake after 60 seconds.  The same
    /// instance is used for both calls because the pubsub backend reuses the
    /// connector across internal reconnects.
    #[rstest]
    #[tokio::test]
    async fn connect_mints_fresh_jwt_per_connect(jwt_secret: JwtSecret) {
        let (listener, port) = free_port_listener().await;

        // Accept two sequential upgrades, capturing the Authorization header
        // from each.  Both captures are driven from a single listener so the
        // test accurately models the pubsub backend reusing one connector.
        let capture = tokio::spawn(async move {
            let auth1 = capture_auth_header_from_listener(&listener).await;
            let auth2 = capture_auth_header_from_listener(&listener).await;
            (auth1, auth2)
        });

        let conn = JwtWsConnect::new(format!("ws://127.0.0.1:{port}").parse().unwrap(), jwt_secret);

        conn.connect().await.expect("first connect must succeed");

        // Advance the clock so the `iat` of the second token is strictly greater.
        tokio::time::sleep(Duration::from_secs(1)).await;

        conn.connect().await.expect("second connect must succeed");

        let (auth1, auth2) = capture.await.unwrap();
        assert_ne!(auth1, auth2, "each connect must produce a fresh JWT");
        let t1 = auth1.strip_prefix("Bearer ").unwrap();
        let t2 = auth2.strip_prefix("Bearer ").unwrap();
        jwt_secret.validate(t1).expect("first token must pass validation");
        jwt_secret.validate(t2).expect("second token must pass validation");
    }
}
