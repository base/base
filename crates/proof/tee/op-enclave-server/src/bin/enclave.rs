//! Main enclave binary.
//!
//! This is the entry point for the enclave server, matching Go's `cmd/enclave/main.go`.
//! It attempts to listen on vsock first, falling back to HTTP if vsock is unavailable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use jsonrpsee::server::Server as JsonRpcServer;

use op_enclave_server::Server;
use op_enclave_server::rpc::{EnclaveApiServer, RpcServerImpl};
use op_enclave_server::transport::TransportConfig;

/// Read timeout for vsock connections (5 minutes).
const VSOCK_READ_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Create the core server
    let server = Arc::new(Server::new()?);
    let config = TransportConfig::default();

    tracing::info!(
        address = %server.signer_address(),
        local_mode = server.is_local_mode(),
        "enclave server initialized"
    );

    // Try vsock first, fall back to HTTP
    #[cfg(unix)]
    {
        if let Err(e) = try_vsock_server(Arc::clone(&server), &config).await {
            tracing::warn!("vsock unavailable, falling back to HTTP: {e}");
            run_http_server(server, &config).await?;
        }
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        tracing::info!("vsock not supported on this platform, using HTTP mode");
        run_http_server(server, &config).await
    }
}

/// Try to start a vsock server.
///
/// This uses jsonrpsee with a custom tower service layer for vsock transport.
#[cfg(unix)]
async fn try_vsock_server(
    server: Arc<Server>,
    config: &TransportConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::io::{Read, Write};
    use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

    let addr = VsockAddr::new(VMADDR_CID_ANY, config.vsock_port);
    let listener = VsockListener::bind(&addr)?;

    tracing::info!(port = config.vsock_port, "listening on vsock");

    let rpc_impl = RpcServerImpl::new(server);
    let module = rpc_impl.into_rpc();

    // Process connections synchronously (vsock is blocking)
    loop {
        let (mut stream, peer_addr) = listener.accept()?;

        // Set read timeout to prevent hanging on slow/stuck clients
        if let Err(e) = stream.set_read_timeout(Some(VSOCK_READ_TIMEOUT)) {
            tracing::warn!("failed to set vsock read timeout: {e}");
        }

        tracing::debug!(
            cid = peer_addr.cid(),
            port = peer_addr.port(),
            "accepted vsock connection"
        );

        let module = module.clone();

        // Handle each connection in a blocking task
        tokio::task::spawn_blocking(move || {
            // Read the request
            let mut buffer = vec![0u8; 50 * 1024 * 1024]; // 50MB buffer (matches HTTP body limit)
            let mut total_read = 0;

            // Read until we have a complete JSON-RPC request
            // Simple approach: read until we find a complete JSON object
            loop {
                match stream.read(&mut buffer[total_read..]) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        total_read += n;
                        // Check if we have a complete JSON object
                        if let Ok(s) = std::str::from_utf8(&buffer[..total_read]) {
                            if s.ends_with('\n') || s.ends_with('}') {
                                // Try to parse as JSON
                                if serde_json::from_str::<serde_json::Value>(s).is_ok() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        tracing::warn!("vsock read timeout");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("vsock read error: {e}");
                        return;
                    }
                }
            }

            if total_read == 0 {
                return;
            }

            // Convert to string for the RPC module
            let request_str = match std::str::from_utf8(&buffer[..total_read]) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("invalid UTF-8 in request: {e}");
                    return;
                }
            };

            // Use tokio runtime to process the async RPC call
            let rt = tokio::runtime::Handle::current();
            let response = rt.block_on(async {
                match module.raw_json_request(request_str, 1).await {
                    Ok((response, _)) => response,
                    Err(e) => {
                        // Return JSON-RPC error response
                        format!(
                            r#"{{"jsonrpc":"2.0","error":{{"code":-32700,"message":"{}"}},"id":null}}"#,
                            e.to_string().replace('"', "\\\"")
                        )
                    }
                }
            });

            // Write the response
            if let Err(e) = stream.write_all(response.as_bytes()) {
                tracing::warn!("vsock write error: {e}");
            }
        });
    }
}

/// Run the HTTP server.
async fn run_http_server(
    server: Arc<Server>,
    config: &TransportConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));

    tracing::info!(
        port = config.http_port,
        body_limit = config.http_body_limit,
        "listening on HTTP"
    );

    let rpc_impl = RpcServerImpl::new(server);

    // Build the JSON-RPC server with body limit
    let server = JsonRpcServer::builder()
        .max_request_body_size(config.http_body_limit)
        .build(addr)
        .await?;

    let handle = server.start(rpc_impl.into_rpc());

    // Wait for server to finish
    handle.stopped().await;

    Ok(())
}
