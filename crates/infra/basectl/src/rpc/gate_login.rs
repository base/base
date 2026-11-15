//! Browser login for the approval gateway: a one-shot loopback HTTP listener
//! that receives `{token, expiresAt}` from the gateway's login page.
//!
//! Each login has a random `state` carried in the callback URL's query. The
//! listener only accepts a callback that echoes it, so another web page or
//! local process can't plant a token by posting to the loopback port.

use std::{fmt, sync::Arc, time::Duration};

use alloy_primitives::{B128, hex};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use url::Url;

use crate::{GateError, GateSettings, TokenCache};

/// A token received from the login page, with its optional expiry.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceivedToken {
    /// The bearer token. Never logged or printed.
    pub token: String,
    /// When the token expires, if the login page said.
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for ReceivedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceivedToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct CallbackBody {
    #[serde(default)]
    token: String,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
}

/// One-shot loopback listener for the login callback.
pub struct LoginListener {
    listener: TcpListener,
    /// Origin of the login page, echoed in CORS headers.
    pub origin: String,
    /// Per-login secret the callback must echo in its `state` query.
    state: String,
}

impl fmt::Debug for LoginListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginListener")
            .field("listener", &self.listener)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl LoginListener {
    /// Path the login page posts the token to.
    pub const CALLBACK_PATH: &str = "/callback";
    /// Query parameter carrying the per-login state.
    pub const STATE_PARAM: &str = "state";
    /// Largest accepted callback body.
    pub const MAX_BODY_BYTES: usize = 64 * 1024;
    /// How long to wait for the login page by default.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
    /// How long one connection may take to send its request, so an idle
    /// connection can't hold up the real callback.
    pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
    /// Pause after a failed accept (for example out of file descriptors),
    /// so a persistent error doesn't spin.
    pub const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);
    const MAX_HEADER_BYTES: usize = 16 * 1024;
    const RECEIVED: &str = "Token received. You can close this tab.";

    /// Accepts `https://` login URLs, or `http://` on a loopback host.
    pub fn accepts(token_url: &Url) -> bool {
        GateSettings::is_secure_or_loopback(token_url)
    }

    /// `scheme://host[:port]` of `token_url`, without path or query.
    pub fn origin_of(token_url: &Url) -> String {
        token_url.origin().ascii_serialization()
    }

    /// `token_url` with `callback_url` appended as a query parameter.
    pub fn login_url(token_url: &Url, callback_url: &str) -> Url {
        let mut url = token_url.clone();
        url.query_pairs_mut().append_pair("callback_url", callback_url);
        url
    }

    /// Binds `127.0.0.1:0` for a login started from `token_url`, with a new
    /// random 128-bit state from the OS random number generator.
    pub async fn bind(token_url: &Url) -> Result<Self, GateError> {
        let state = B128::try_random()
            .map_err(|error| GateError::Token(format!("login state: {error}")))?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| GateError::Token(format!("login listener: {error}")))?;
        Ok(Self { listener, origin: Self::origin_of(token_url), state: hex::encode(state) })
    }

    /// `http://127.0.0.1:<port>/callback?state=<32 hex chars>`.
    pub fn callback_url(&self) -> Result<String, GateError> {
        let port = self
            .listener
            .local_addr()
            .map_err(|error| GateError::Token(format!("login listener: {error}")))?
            .port();
        Ok(format!(
            "http://127.0.0.1:{port}{}?{}={}",
            Self::CALLBACK_PATH,
            Self::STATE_PARAM,
            self.state
        ))
    }

    /// Whether `given` equals this login's state, compared in constant time.
    fn state_matches(&self, given: Option<&str>) -> bool {
        let expected = self.state.as_bytes();
        let Some(given) = given.map(str::as_bytes) else { return false };
        given.len() == expected.len()
            && given.iter().zip(expected).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
    }

    /// Serves requests until a valid token arrives or `timeout` passes.
    /// Callbacks with a missing or wrong state get a 403 and are ignored.
    /// Each connection is served on its own task with
    /// [`Self::CONNECTION_TIMEOUT`]; tasks still running when this returns
    /// are aborted.
    pub async fn wait(self, timeout: Duration) -> Result<ReceivedToken, GateError> {
        let this = Arc::new(self);
        let serve = async {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    // Match any finished task: a refutable pattern would disable this
                    // branch for the rest of the `select!` after a rejected request,
                    // leaving a later valid callback unjoined. An empty set yields
                    // `None`, which disables the branch and waits on `accept`.
                    Some(joined) = connections.join_next() => {
                        if let Ok(Some(token)) = joined {
                            return token;
                        }
                    }
                    accepted = this.listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let this = Arc::clone(&this);
                            connections.spawn(async move {
                                tokio::time::timeout(Self::CONNECTION_TIMEOUT, this.handle(stream))
                                    .await
                                    .ok()
                                    .flatten()
                            });
                        }
                        Err(_) => tokio::time::sleep(Self::ACCEPT_RETRY_DELAY).await,
                    },
                }
            }
        };
        tokio::time::timeout(timeout, serve)
            .await
            .map_err(|_| GateError::Token("timed out waiting for the browser login".to_string()))
    }

    /// Handles one request; returns the token on a successful callback.
    async fn handle(&self, mut stream: TcpStream) -> Option<ReceivedToken> {
        let mut buf = Vec::new();
        let header_end = loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            if buf.len() > Self::MAX_HEADER_BYTES {
                return None;
            }
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_string();
        let target = request_line.next()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let state = url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == Self::STATE_PARAM)
            .map(|(_, value)| value.into_owned());
        let content_length = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);

        if method == "OPTIONS" {
            self.respond(&mut stream, "204 No Content", "").await;
            return None;
        }
        if method != "POST" || path != Self::CALLBACK_PATH {
            self.respond(&mut stream, "404 Not Found", "not found").await;
            return None;
        }
        if content_length > Self::MAX_BODY_BYTES {
            self.respond(&mut stream, "413 Payload Too Large", "body too large").await;
            return None;
        }
        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let mut chunk = vec![0u8; content_length - body.len()];
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        // The body is read first so the 403 isn't lost to a connection reset.
        // The content type isn't checked: the login page posts `text/plain`
        // without CORS, and the state is what proves the sender.
        if !self.state_matches(state.as_deref()) {
            self.respond(&mut stream, "403 Forbidden", "invalid state").await;
            return None;
        }
        let parsed = serde_json::from_slice::<CallbackBody>(&body).ok();
        match parsed.filter(|parsed| !parsed.token.trim().is_empty()) {
            Some(parsed) => {
                self.respond(&mut stream, "200 OK", Self::RECEIVED).await;
                Some(ReceivedToken {
                    token: parsed.token.trim().to_string(),
                    expires_at: parsed.expires_at.as_ref().and_then(TokenCache::parse_expires_at),
                })
            }
            None => {
                self.respond(&mut stream, "400 Bad Request", "token required").await;
                None
            }
        }
    }

    async fn respond(&self, stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\n\
             Access-Control-Allow-Origin: {}\r\n\
             Access-Control-Allow-Methods: POST, OPTIONS\r\n\
             Access-Control-Allow-Headers: Content-Type\r\n\
             Access-Control-Allow-Private-Network: true\r\n\
             Vary: Origin\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            self.origin,
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::Duration,
    };

    use url::Url;

    use super::LoginListener;

    fn raw(callback: &str, request: &str) -> String {
        let address = Url::parse(callback).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", address.port().unwrap())).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// Posts `body` to `target` (path and query) with `content_type`.
    fn post_to(callback: &str, target: &str, content_type: &str, body: &str) -> String {
        raw(
            callback,
            &format!(
                "POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    /// Path and query of `callback`, as the login page posts to it.
    fn target(callback: &str) -> String {
        let url = Url::parse(callback).unwrap();
        format!("{}?{}", url.path(), url.query().unwrap())
    }

    fn post(callback: &str, body: &str) -> String {
        post_to(callback, &target(callback), "application/json", body)
    }

    async fn listen()
    -> (String, tokio::task::JoinHandle<Result<super::ReceivedToken, crate::GateError>>) {
        let token_url = Url::parse("https://login.example/token").unwrap();
        let listener = LoginListener::bind(&token_url).await.unwrap();
        let callback = listener.callback_url().unwrap();
        (callback, tokio::spawn(listener.wait(Duration::from_secs(10))))
    }

    #[test]
    fn origin_keeps_port_and_drops_path_and_query() {
        let url = Url::parse("https://login.example:8443/a/b?x=1").unwrap();
        assert_eq!(LoginListener::origin_of(&url), "https://login.example:8443");
        let url = Url::parse("https://login.example/token").unwrap();
        assert_eq!(LoginListener::origin_of(&url), "https://login.example");
    }

    #[test]
    fn login_url_appends_encoded_callback() {
        let url = Url::parse("https://login.example/token?app=x").unwrap();
        assert_eq!(
            LoginListener::login_url(&url, "http://127.0.0.1:5/callback").as_str(),
            "https://login.example/token?app=x&callback_url=http%3A%2F%2F127.0.0.1%3A5%2Fcallback"
        );
        assert!(LoginListener::accepts(&Url::parse("http://127.0.0.1:9/t").unwrap()));
        assert!(!LoginListener::accepts(&Url::parse("http://login.example/t").unwrap()));
    }

    #[tokio::test]
    async fn callback_url_carries_a_fresh_32_hex_state() {
        let token_url = Url::parse("https://login.example/token").unwrap();
        let mut states = Vec::new();
        for _ in 0..2 {
            let callback = LoginListener::bind(&token_url).await.unwrap().callback_url().unwrap();
            let url = Url::parse(&callback).unwrap();
            assert_eq!(url.path(), LoginListener::CALLBACK_PATH);
            let pairs: Vec<_> = url.query_pairs().collect();
            assert_eq!(pairs.len(), 1, "{callback}");
            let (key, state) = &pairs[0];
            assert_eq!(key, "state");
            assert!(
                state.len() == 32 && state.bytes().all(|b| b.is_ascii_hexdigit()),
                "{callback}"
            );
            states.push(state.to_string());
        }
        assert_ne!(states[0], states[1]);
    }

    #[tokio::test]
    async fn listener_rejects_missing_or_wrong_state_then_accepts_the_right_one() {
        let (callback, waiting) = listen().await;

        let responses = tokio::task::spawn_blocking(move || {
            let body = r#"{"token":"planted"}"#;
            let missing = post_to(&callback, "/callback", "application/json", body);
            let other_query = post_to(&callback, "/callback?x=1", "application/json", body);
            let empty = post_to(&callback, "/callback?state=", "application/json", body);
            let wrong = post_to(
                &callback,
                "/callback?state=00000000000000000000000000000000",
                "text/plain",
                body,
            );
            let ok = post(&callback, r#"{"token":"tok"}"#);
            (missing, other_query, empty, wrong, ok)
        })
        .await
        .unwrap();

        let (missing, other_query, empty, wrong, ok) = responses;
        for rejected in [&missing, &other_query, &empty, &wrong] {
            assert!(rejected.starts_with("HTTP/1.1 403"), "{rejected}");
            assert!(rejected.contains("Access-Control-Allow-Origin: https://login.example\r\n"));
        }
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let received = waiting.await.unwrap().unwrap();
        assert_eq!(received.token, "tok");
    }

    #[tokio::test]
    async fn idle_connection_does_not_block_the_callback() {
        let (callback, waiting) = listen().await;

        let ok = tokio::task::spawn_blocking(move || {
            let port = Url::parse(&callback).unwrap().port().unwrap();
            // Connects and sends nothing, like a speculative browser connection.
            let idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let partial = {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                stream.write_all(b"POST /callback HTTP/1.1\r\n").unwrap();
                stream
            };
            let ok = post(&callback, r#"{"token":"tok"}"#);
            drop((idle, partial));
            ok
        })
        .await
        .unwrap();

        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert_eq!(waiting.await.unwrap().unwrap().token, "tok");
    }

    #[tokio::test]
    async fn rejected_request_finishing_first_does_not_stall_the_callback() {
        let token_url = Url::parse("https://login.example/token").unwrap();
        let listener = LoginListener::bind(&token_url).await.unwrap();
        let callback = listener.callback_url().unwrap();
        let waiting = tokio::spawn(listener.wait(Duration::from_secs(3)));

        let (rejected, ok) = tokio::task::spawn_blocking(move || {
            let port = Url::parse(&callback).unwrap().port().unwrap();
            // The valid callback connects first and sends only its request line.
            let mut valid = TcpStream::connect(("127.0.0.1", port)).unwrap();
            valid.write_all(format!("POST {} HTTP/1.1\r\n", target(&callback)).as_bytes()).unwrap();
            // A wrong-state request is accepted after it and completes first.
            let rejected = post_to(
                &callback,
                "/callback?state=00000000000000000000000000000000",
                "application/json",
                r#"{"token":"planted"}"#,
            );
            std::thread::sleep(Duration::from_millis(200));
            let body = r#"{"token":"tok"}"#;
            valid
                .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
                .unwrap();
            let mut ok = String::new();
            valid.read_to_string(&mut ok).unwrap();
            (rejected, ok)
        })
        .await
        .unwrap();

        assert!(rejected.starts_with("HTTP/1.1 403"), "{rejected}");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let received = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("wait returns once the valid callback is answered")
            .unwrap()
            .unwrap();
        assert_eq!(received.token, "tok");
    }

    #[tokio::test]
    async fn listener_accepts_text_plain_body_with_the_right_state() {
        let (callback, waiting) = listen().await;

        let ok = tokio::task::spawn_blocking(move || {
            post_to(
                &callback,
                &target(&callback),
                "text/plain;charset=UTF-8",
                r#"{"token":"tok","expiresAt":"2999-01-01T00:00:00Z"}"#,
            )
        })
        .await
        .unwrap();

        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let received = waiting.await.unwrap().unwrap();
        assert_eq!(received.token, "tok");
        assert!(received.expires_at.is_some());
    }

    #[tokio::test]
    async fn listener_answers_preflight_rejects_bad_bodies_and_accepts_token() {
        let token_url = Url::parse("https://login.example:8443/token?x=1").unwrap();
        let listener = LoginListener::bind(&token_url).await.unwrap();
        let callback = listener.callback_url().unwrap();
        let waiting = tokio::spawn(listener.wait(Duration::from_secs(10)));

        let responses = tokio::task::spawn_blocking(move || {
            let preflight = raw(&callback, "OPTIONS /callback HTTP/1.1\r\nHost: x\r\n\r\n");
            let empty = post(&callback, r#"{"token":""}"#);
            let oversized = raw(
                &callback,
                &format!(
                    "POST /callback HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                    LoginListener::MAX_BODY_BYTES + 1
                ),
            );
            let other = raw(&callback, "GET /other HTTP/1.1\r\n\r\n");
            let ok = post(&callback, r#"{"token":"tok","expiresAt":1900000000}"#);
            (preflight, empty, oversized, other, ok)
        })
        .await
        .unwrap();

        let (preflight, empty, oversized, other, ok) = responses;
        assert!(preflight.starts_with("HTTP/1.1 204"));
        for header in [
            "Access-Control-Allow-Origin: https://login.example:8443\r\n",
            "Access-Control-Allow-Methods: POST, OPTIONS\r\n",
            "Access-Control-Allow-Headers: Content-Type\r\n",
            "Access-Control-Allow-Private-Network: true\r\n",
            "Vary: Origin\r\n",
        ] {
            assert!(preflight.contains(header), "missing {header}");
        }
        assert!(!preflight.contains("Allow-Origin: *"));
        assert!(empty.starts_with("HTTP/1.1 400") && empty.contains("Allow-Origin: https://"));
        assert!(oversized.starts_with("HTTP/1.1 413"));
        assert!(other.starts_with("HTTP/1.1 404"));
        assert!(
            ok.starts_with("HTTP/1.1 200")
                && ok.ends_with("Token received. You can close this tab.")
        );

        let received = waiting.await.unwrap().unwrap();
        assert_eq!(received.token, "tok");
        assert_eq!(received.expires_at.unwrap().timestamp(), 1_900_000_000);
    }
}
