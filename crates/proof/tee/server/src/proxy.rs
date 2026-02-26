//! HTTP-to-vsock proxy.
//!
//! Forwards HTTP requests to the enclave over vsock transport.

#[cfg(unix)]
use std::io::{Read, Write};
use std::{net::SocketAddr, sync::Arc, time::Duration};

use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode, body::Bytes, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

/// Maximum HTTP body size (50 MB, matching enclave-side limit).
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// Read timeout for vsock connections (5 minutes, matching enclave-side timeout).
#[cfg(unix)]
const VSOCK_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Run the HTTP-to-vsock proxy.
#[cfg(unix)]
pub async fn run(
    cid: u32,
    vsock_port: u32,
    http_port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([0, 0, 0, 0], http_port));

    info!(
        http_port = http_port,
        vsock_cid = cid,
        vsock_port = vsock_port,
        "starting HTTP proxy to vsock"
    );

    let pool = Arc::new(VsockPool::new(cid, vsock_port));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, peer_addr) = listener.accept().await?;

        debug!(peer = %peer_addr, "accepted connection");

        let pool = Arc::clone(&pool);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);

            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let pool = Arc::clone(&pool);
                async move { handle_request(req, pool).await }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                warn!(error = %e, "connection error");
            }
        });
    }
}

/// Run the HTTP-to-vsock proxy (unsupported on non-Unix platforms).
#[cfg(not(unix))]
pub async fn run(
    _cid: u32,
    _vsock_port: u32,
    _http_port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("vsock proxy is only supported on Unix platforms".into())
}

/// Connection pool for vsock connections.
#[cfg(unix)]
struct VsockPool {
    connections: parking_lot::Mutex<std::collections::VecDeque<vsock::VsockStream>>,
    cid: u32,
    port: u32,
}

#[cfg(unix)]
impl VsockPool {
    const fn new(cid: u32, port: u32) -> Self {
        Self { connections: parking_lot::Mutex::new(std::collections::VecDeque::new()), cid, port }
    }

    /// Get a connection from the pool or create a new one.
    fn get(&self) -> Option<vsock::VsockStream> {
        if let Some(conn) = self.connections.lock().pop_front() {
            return Some(conn);
        }
        vsock::VsockStream::connect(&vsock::VsockAddr::new(self.cid, self.port)).ok()
    }

    /// Return a connection to the pool.
    fn put(&self, conn: vsock::VsockStream) {
        let mut conns = self.connections.lock();
        // Limit pool size to prevent unbounded growth.
        if conns.len() < 10 {
            conns.push_back(conn);
        }
    }
}

/// Handle an HTTP request by forwarding it to vsock.
#[cfg(unix)]
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    pool: Arc<VsockPool>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Full::new(Bytes::new()))
            .expect("failed to build response"));
    }

    let limited = Limited::new(req.into_body(), MAX_BODY_SIZE);
    let body = match limited.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(Full::new(Bytes::new()))
                .expect("failed to build response"));
        }
    };

    let response = tokio::task::spawn_blocking(move || forward_to_vsock(&pool, &body))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "spawn_blocking error");
        })
        .ok()
        .flatten();

    response.map_or_else(
        || {
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::new()))
                .expect("failed to build response"))
        },
        |data| {
            Ok(Response::builder()
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(data)))
                .expect("failed to build response"))
        },
    )
}

/// Forward a request to vsock and return the response.
#[cfg(unix)]
fn forward_to_vsock(pool: &VsockPool, request: &[u8]) -> Option<Vec<u8>> {
    let mut conn = pool.get()?;

    if let Err(e) = conn.set_read_timeout(Some(VSOCK_READ_TIMEOUT)) {
        warn!(error = %e, "failed to set vsock read timeout");
    }

    if conn.write_all(request).is_err() {
        return None;
    }

    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];

    loop {
        match conn.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                if let Ok(s) = std::str::from_utf8(&response)
                    && serde_json::from_str::<serde_json::Value>(s).is_ok()
                {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                warn!("vsock read timeout");
                break;
            }
            Err(_) => break,
        }
    }

    if response.is_empty() {
        None
    } else {
        pool.put(conn);
        Some(response)
    }
}
