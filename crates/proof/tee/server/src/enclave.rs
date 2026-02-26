//! Enclave server runtime.
//!
//! Tries vsock transport first, falls back to HTTP.

#[cfg(unix)]
use std::io::{Read, Write};
use std::{net::SocketAddr, sync::Arc, time::Duration};

use jsonrpsee::server::{ServerBuilder, ServerConfig};
use serde_json::value::RawValue;
use tracing::{debug, info, warn};
#[cfg(unix)]
use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

use crate::{
    Server,
    rpc::{EnclaveApiServer, RpcServerImpl},
    transport::TransportConfig,
};

/// Read timeout for vsock connections (5 minutes).
const VSOCK_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Run the enclave server. Tries vsock first, falls back to HTTP.
pub async fn run(config: TransportConfig) -> eyre::Result<()> {
    let server = Arc::new(Server::new()?);

    info!(
        address = %server.signer_address(),
        local_mode = server.is_local_mode(),
        "enclave server initialized"
    );

    // Try vsock first, fall back to HTTP.
    #[cfg(unix)]
    {
        if let Err(e) = try_vsock_server(Arc::clone(&server), &config).await {
            warn!(error = %e, "vsock unavailable, falling back to HTTP");
            run_http_server(server, &config).await?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        info!("vsock not supported on this platform, using HTTP mode");
        run_http_server(server, &config).await?;
        Ok(())
    }
}

/// Try to start a vsock server.
#[cfg(unix)]
async fn try_vsock_server(server: Arc<Server>, config: &TransportConfig) -> eyre::Result<()> {
    let addr = VsockAddr::new(VMADDR_CID_ANY, config.vsock_port);
    let listener = VsockListener::bind(&addr)?;

    info!(port = config.vsock_port, "listening on vsock");

    let rpc_impl = RpcServerImpl::new(server);
    let module = rpc_impl.into_rpc();

    // Process connections synchronously (vsock is blocking).
    loop {
        let (mut stream, peer_addr) = listener.accept()?;

        // Set read timeout to prevent hanging on slow/stuck clients.
        if let Err(e) = stream.set_read_timeout(Some(VSOCK_READ_TIMEOUT)) {
            warn!(error = %e, "failed to set vsock read timeout");
        }

        debug!(cid = peer_addr.cid(), port = peer_addr.port(), "accepted vsock connection");

        let module = module.clone();

        // Handle each connection in a blocking task.
        tokio::task::spawn_blocking(move || {
            // Read the request.
            let mut buffer = vec![0u8; 50 * 1024 * 1024]; // 50MB buffer
            let mut total_read = 0;

            loop {
                match stream.read(&mut buffer[total_read..]) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        total_read += n;
                        if let Ok(s) = std::str::from_utf8(&buffer[..total_read])
                            && (s.ends_with('\n') || s.ends_with('}'))
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
                        return;
                    }
                    Err(e) => {
                        warn!(error = %e, "vsock read error");
                        return;
                    }
                }
            }

            if total_read == 0 {
                return;
            }

            let request_str = match std::str::from_utf8(&buffer[..total_read]) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "invalid UTF-8 in request");
                    return;
                }
            };

            let rt = tokio::runtime::Handle::current();
            let response = rt.block_on(async {
                match module.raw_json_request(request_str, 1).await {
                    Ok((response, _)) => response,
                    Err(e) => {
                        let msg = e.to_string().replace('"', "\\\"");
                        RawValue::from_string(format!(
                            r#"{{"jsonrpc":"2.0","error":{{"code":-32700,"message":"{msg}"}},"id":null}}"#,
                        ))
                        .expect("valid JSON-RPC error")
                    }
                }
            });

            if let Err(e) = stream.write_all(response.get().as_bytes()) {
                warn!(error = %e, "vsock write error");
            }
        });
    }
}

/// Run the HTTP server.
async fn run_http_server(server: Arc<Server>, config: &TransportConfig) -> eyre::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));

    info!(port = config.http_port, body_limit = config.http_body_limit, "listening on HTTP");

    let rpc_impl = RpcServerImpl::new(server);

    let server_config =
        ServerConfig::builder().max_request_body_size(config.http_body_limit).build();
    let server = ServerBuilder::with_config(server_config).build(addr).await?;

    let handle = server.start(rpc_impl.into_rpc());
    handle.stopped().await;

    Ok(())
}
