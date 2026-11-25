//! A small HTTP/1.1 JSON-RPC server: one thread per connection (bounded), keep-alive, and JWT
//! (HS256) authentication on the auth port, as the Engine API requires.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;

use crate::Node;

/// Most connections served at once per server.
const MAX_CONNECTIONS: usize = 256;
/// Largest request body accepted (base-reth's default is 15 MB).
const MAX_BODY: usize = 16 << 20;
/// Largest header section accepted.
const MAX_HEADER_LINE: usize = 16 << 10;
/// How far a JWT `iat` may be from now, in seconds (the Engine API allows 60).
const JWT_IAT_WINDOW: u64 = 60;
/// How long an idle keep-alive connection stays open.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// A JWT secret: 32 bytes, read as hex from the `--authrpc.jwtsecret` file.
#[derive(Clone)]
pub struct JwtSecret([u8; 32]);

impl core::fmt::Debug for JwtSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("JwtSecret(..)")
    }
}

impl JwtSecret {
    /// Parses a hex secret (an optional `0x` prefix and surrounding whitespace are allowed).
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let text = text.strip_prefix("0x").unwrap_or(text);
        let bytes = alloy_primitives::hex::decode(text).map_err(|error| format!("JWT secret: {error}"))?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| "JWT secret must be 32 bytes".to_owned())?;
        Ok(Self(bytes))
    }

    /// Reads the secret file, or creates it with a random secret (as base-reth does) when it
    /// does not exist.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_hex(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 32];
                let mut random = std::fs::File::open("/dev/urandom").map_err(|error| error.to_string())?;
                random.read_exact(&mut bytes).map_err(|error| error.to_string())?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(path, alloy_primitives::hex::encode(bytes))
                    .map_err(|error| format!("writing {}: {error}", path.display()))?;
                Ok(Self(bytes))
            }
            Err(error) => Err(format!("reading {}: {error}", path.display())),
        }
    }

    /// Checks a `Bearer` token: HS256 signature and an `iat` within a minute of now.
    pub fn verify(&self, token: &str) -> Result<(), String> {
        let mut parts = token.split('.');
        let (Some(header), Some(claims), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("malformed JWT".into());
        };
        let header: Value = serde_json::from_slice(&base64url_decode(header)?).map_err(|e| e.to_string())?;
        if header.get("alg").and_then(Value::as_str) != Some("HS256") {
            return Err("JWT algorithm is not HS256".into());
        }
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0).expect("HMAC takes any key");
        mac.update(token[..header_len(token)].as_bytes());
        mac.verify_slice(&base64url_decode(signature)?).map_err(|_| "invalid JWT signature".to_owned())?;
        let claims: Value = serde_json::from_slice(&base64url_decode(claims)?).map_err(|e| e.to_string())?;
        let iat = claims.get("iat").and_then(Value::as_u64).ok_or("JWT has no iat")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after 1970").as_secs();
        if now.abs_diff(iat) > JWT_IAT_WINDOW {
            return Err("JWT iat is stale".into());
        }
        Ok(())
    }
}

/// Length of `header.claims` in a JWT (what the signature covers).
fn header_len(token: &str) -> usize {
    token.rfind('.').expect("checked to have three parts")
}

fn base64url_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err("invalid base64url".into()),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

/// Serves JSON-RPC on `address` until the process exits. With `jwt`, every request needs a
/// valid token and may call `engine_*` methods.
pub fn serve(node: Arc<Node>, address: SocketAddr, jwt: Option<JwtSecret>) -> Result<(), String> {
    let listener = TcpListener::bind(address).map_err(|error| format!("binding {address}: {error}"))?;
    let active = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name(format!("rpc-{}", address.port()))
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let (node, jwt, active) = (Arc::clone(&node), jwt.clone(), Arc::clone(&active));
                let spawned = thread::Builder::new().name("rpc-conn".into()).spawn(move || {
                    let _ = connection(&node, stream, jwt.as_ref());
                    active.fetch_sub(1, Ordering::Relaxed);
                });
                if spawned.is_err() {
                    eprintln!("tiger: could not spawn a connection thread");
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

thread_local! {
    /// When this connection thread's current request began arriving (`TIGER_VALIDATION_LOG`).
    static REQUEST_STARTED: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// When the request this thread serves began arriving.
pub(crate) fn request_started() -> Option<std::time::Instant> {
    REQUEST_STARTED.get()
}

fn connection(node: &Node, stream: TcpStream, jwt: Option<&JwtSecret>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.by_ref().take(MAX_HEADER_LINE as u64).read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        REQUEST_STARTED.set(Some(std::time::Instant::now()));
        let method = line.split(' ').next().unwrap_or_default().to_owned();
        let http10 = line.trim_end().ends_with("HTTP/1.0");
        let mut length = 0usize;
        let mut authorization = None;
        let mut close = http10;
        loop {
            let mut header = String::new();
            if reader.by_ref().take(MAX_HEADER_LINE as u64).read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.parse().unwrap_or(usize::MAX),
                "authorization" => authorization = value.strip_prefix("Bearer ").map(str::to_owned),
                "connection" => close = value.eq_ignore_ascii_case("close"),
                _ => {}
            }
        }
        if length > MAX_BODY {
            respond(&mut writer, 413, "Payload Too Large", b"", true)?;
            return Ok(());
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        if method != "POST" {
            respond(&mut writer, 405, "Method Not Allowed", b"", close)?;
        } else if let Some(Err(error)) = jwt.map(|secret| {
            authorization.as_deref().ok_or_else(|| "missing JWT".to_owned()).and_then(|token| secret.verify(token))
        }) {
            respond(&mut writer, 401, "Unauthorized", error.as_bytes(), close)?;
        } else {
            let response = node.handle_body(&body, jwt.is_some());
            // base-reth answers a notification with the body `null`.
            let bytes = serde_json::to_vec(&response.unwrap_or(Value::Null)).expect("JSON serializes");
            respond(&mut writer, 200, "OK", &bytes, close)?;
        }
        if close {
            return Ok(());
        }
    }
}

fn respond(writer: &mut TcpStream, code: u16, reason: &str, body: &[u8], close: bool) -> std::io::Result<()> {
    let connection = if close { "close" } else { "keep-alive" };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: {connection}\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let word = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[((word >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn token(secret: &[u8; 32], iat: u64) -> String {
        let signed = format!(
            "{}.{}",
            base64url(br#"{"alg":"HS256","typ":"JWT"}"#),
            base64url(format!(r#"{{"iat":{iat}}}"#).as_bytes())
        );
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).unwrap();
        mac.update(signed.as_bytes());
        format!("{signed}.{}", base64url(&mac.finalize().into_bytes()))
    }

    #[test]
    fn jwt_accepts_fresh_tokens_and_rejects_stale_or_forged_ones() {
        let secret = JwtSecret([7; 32]);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secret.verify(&token(&[7; 32], now)), Ok(()));
        assert_eq!(secret.verify(&token(&[7; 32], now - 59)), Ok(()));
        assert!(secret.verify(&token(&[7; 32], now - 61)).is_err());
        assert!(secret.verify(&token(&[8; 32], now)).is_err());
    }
}
