//! JSON-RPC fault injection helpers for system tests.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base_builder_core::test_utils::get_available_port;
use eyre::{Result, WrapErr};
use jsonrpsee::{
    RpcModule,
    server::{Server, ServerHandle},
    types::{ErrorObjectOwned, error::ErrorCode},
};
use serde_json::{Value, json};
use tracing::info;
use url::Url;

/// JSON-RPC response fault to apply through a [`FaultedRpcProxy`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RpcFault {
    /// Remove `requestsHash` from `eth_getBlockByNumber` block responses.
    MissingRequestsHash,
}

/// Running JSON-RPC proxy that forwards selected methods and applies configured faults.
///
/// This proxy currently registers the source RPC methods used by
/// `base_consensus_node::RemoteL2Client`: `eth_blockNumber` and
/// `eth_getBlockByNumber`. If follow-mode source RPC usage expands, add explicit
/// forwarding for each new method here so tests fail at the missing method boundary
/// instead of silently bypassing fault injection.
pub struct FaultedRpcProxy {
    rpc_addr: SocketAddr,
    handle: ServerHandle,
}

impl std::fmt::Debug for FaultedRpcProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultedRpcProxy").field("rpc_addr", &self.rpc_addr).finish()
    }
}

impl FaultedRpcProxy {
    /// Starts a faulted JSON-RPC proxy against the given upstream endpoint.
    pub async fn start(upstream_rpc_url: Url, faults: Vec<RpcFault>) -> Result<Self> {
        let rpc_port = get_available_port();
        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .wrap_err("failed to build faulted RPC proxy client")?;

        let mut module = RpcModule::new(());
        let request_id_counter = Arc::new(AtomicU64::new(1));

        let block_number_client = client.clone();
        let block_number_upstream = upstream_rpc_url.clone();
        let block_number_request_id_counter = Arc::clone(&request_id_counter);
        module
            .register_async_method("eth_blockNumber", move |_, _, _| {
                let client = block_number_client.clone();
                let upstream = block_number_upstream.clone();
                let request_id = Self::next_request_id(&block_number_request_id_counter);
                async move {
                    Self::forward_rpc(&client, &upstream, request_id, "eth_blockNumber", json!([]))
                        .await
                }
            })
            .wrap_err("failed to register eth_blockNumber proxy method")?;

        let block_by_number_client = client;
        let block_by_number_upstream = upstream_rpc_url;
        let block_by_number_faults = faults;
        let block_by_number_request_id_counter = request_id_counter;
        module
            .register_async_method("eth_getBlockByNumber", move |params, _, _| {
                let client = block_by_number_client.clone();
                let upstream = block_by_number_upstream.clone();
                let faults = block_by_number_faults.clone();
                let request_id = Self::next_request_id(&block_by_number_request_id_counter);
                async move {
                    let params = params
                        .parse::<Vec<Value>>()
                        .map_err(|e| Self::invalid_params_error(format!("invalid params: {e}")))?;
                    let mut result = Self::forward_rpc(
                        &client,
                        &upstream,
                        request_id,
                        "eth_getBlockByNumber",
                        json!(params),
                    )
                    .await?;
                    Self::apply_block_response_faults(&mut result, &faults);
                    Ok::<_, ErrorObjectOwned>(result)
                }
            })
            .wrap_err("failed to register eth_getBlockByNumber proxy method")?;

        let server =
            Server::builder().build(rpc_addr).await.wrap_err("failed to bind faulted RPC proxy")?;
        let rpc_addr = server.local_addr().wrap_err("failed to read faulted RPC proxy addr")?;
        let handle = server.start(module);

        info!(rpc_port = rpc_addr.port(), "faulted RPC proxy started");
        Ok(Self { rpc_addr, handle })
    }

    /// Returns the next upstream request ID for this proxy.
    pub fn next_request_id(counter: &AtomicU64) -> Value {
        Value::from(counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the RPC URL for this proxy.
    pub fn rpc_url(&self) -> Url {
        Url::parse(&format!("http://{}:{}", self.rpc_addr.ip(), self.rpc_addr.port()))
            .expect("valid RPC URL")
    }

    /// Forwards a JSON-RPC request to the upstream endpoint.
    pub async fn forward_rpc(
        client: &reqwest::Client,
        upstream: &Url,
        request_id: Value,
        method: &str,
        params: Value,
    ) -> Result<Value, ErrorObjectOwned> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });
        let response = client
            .post(upstream.clone())
            .json(&request)
            .send()
            .await
            .map_err(|e| Self::internal_error(format!("upstream request failed: {e}")))?
            .json::<Value>()
            .await
            .map_err(|e| Self::internal_error(format!("upstream response decode failed: {e}")))?;

        if let Some(error) = response.get("error") {
            return Err(Self::internal_error(format!("upstream RPC error for {method}: {error}")));
        }

        response.get("result").cloned().ok_or_else(|| {
            Self::internal_error(format!("upstream response for {method} omitted result"))
        })
    }

    /// Applies configured faults to an `eth_getBlockByNumber` response value.
    pub fn apply_block_response_faults(value: &mut Value, faults: &[RpcFault]) {
        for fault in faults {
            match fault {
                RpcFault::MissingRequestsHash => Self::remove_requests_hash(value),
            }
        }
    }

    /// Removes `requestsHash` from a block response value.
    pub fn remove_requests_hash(value: &mut Value) {
        if let Some(block) = value.as_object_mut() {
            block.remove("requestsHash");
        }
    }

    /// Builds a JSON-RPC internal error object.
    pub fn internal_error(message: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ErrorCode::InternalError.code(), message, None::<()>)
    }

    /// Builds a JSON-RPC invalid params error object.
    pub fn invalid_params_error(message: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), message, None::<()>)
    }
}

impl Drop for FaultedRpcProxy {
    fn drop(&mut self) {
        let _ = self.handle.stop();
    }
}
