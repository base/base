//! The running node: the chain, the Engine API state, and JSON-RPC dispatch.

use std::{
    path::Path,
    sync::{Mutex, OnceLock, RwLock},
};

use base_tiger_evm::ChainSpec;
use serde_json::{Value, json};

use crate::{Chain, Genesis, engine::EngineState};

/// A JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// The error code.
    pub code: i64,
    /// The message.
    pub message: String,
    /// Extra data (revert output, for example).
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with `code` and `message`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    /// `-32602`.
    pub fn invalid_params(message: String) -> Self {
        Self::new(-32602, message)
    }

    /// `-32601`.
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("the method {method} does not exist/is not available"))
    }

    /// `-32603`.
    pub fn internal(message: String) -> Self {
        Self::new(-32603, message)
    }

    /// The JSON error object.
    pub fn to_json(&self) -> Value {
        match &self.data {
            Some(data) => json!({ "code": self.code, "message": self.message, "data": data }),
            None => json!({ "code": self.code, "message": self.message }),
        }
    }
}

/// The node.
#[derive(Debug)]
pub struct Node {
    /// The chain's parameters.
    pub spec: ChainSpec,
    /// The block tree and state.
    pub chain: RwLock<Chain>,
    /// Engine API state; its lock also serializes Engine API calls.
    pub engine: Mutex<EngineState>,
    /// The network and sync thread, once started.
    pub sync: OnceLock<crate::sync::Sync>,
    /// JSON-RPC filters installed with `eth_newFilter` and `eth_newBlockFilter`.
    pub filters: crate::RpcFilters,
    /// JSON-RPC options from the command line.
    pub rpc_config: crate::RpcConfig,
    /// The upstream RPC a stateless node reads witnesses (and RPC answers for state it does not
    /// hold) from.
    pub upstream: Option<crate::Upstream>,
    /// Execution witnesses fetched ahead from the upstream (a stateless node).
    pub witnesses: Option<crate::Witnesses>,
    /// The prewarm queue of a stateless node, once started.
    pub prewarm: OnceLock<crate::Prewarm>,
    /// The persistence thread of a full state, once started.
    pub persistence: crate::Persistence,
}

impl Node {
    /// Opens the node's datadir. With `upstream`, a new datadir starts stateless at the
    /// upstream's block `start` (a number or tag), and missing state is read from the upstream.
    pub fn open(datadir: &Path, genesis: Genesis, upstream: Option<(crate::Upstream, String)>) -> Result<Self, String> {
        let spec = genesis.chain.clone();
        let stateless = upstream.as_ref().map(|(upstream, block)| crate::StatelessStart { upstream, block });
        let chain = Chain::open(datadir, genesis, stateless)?;
        Ok(Self {
            spec,
            chain: RwLock::new(chain),
            engine: Mutex::new(EngineState::default()),
            sync: OnceLock::new(),
            filters: crate::RpcFilters::default(),
            rpc_config: crate::RpcConfig::default(),
            prewarm: OnceLock::new(),
            persistence: crate::Persistence::default(),
            witnesses: upstream.as_ref().map(|(upstream, _)| crate::Witnesses::new(upstream)),
            upstream: upstream.map(|(upstream, _)| upstream),
        })
    }

    /// Answers one JSON-RPC call. `authenticated` calls (the auth port) may use `engine_*`.
    pub fn call(&self, method: &str, params: &Value, authenticated: bool) -> Result<Value, RpcError> {
        if method.starts_with("engine_") {
            if !authenticated {
                return Err(RpcError::method_not_found(method));
            }
            return self.engine(method, params);
        }
        self.rpc(method, params)
    }

    /// Answers one JSON-RPC request object (or batch), returning the response, or `None` for a
    /// batch of notifications.
    pub fn handle(&self, request: &Value, authenticated: bool) -> Option<Value> {
        if let Value::Array(batch) = request {
            if batch.is_empty() {
                return Some(Self::error_response(Value::Null, &RpcError::new(-32600, "empty batch")));
            }
            return Some(Value::Array(batch.iter().filter_map(|one| self.handle_one(one, authenticated)).collect()));
        }
        self.handle_one(request, authenticated)
    }

    fn handle_one(&self, request: &Value, authenticated: bool) -> Option<Value> {
        let id = request.get("id").cloned();
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return Some(Self::error_response(id.unwrap_or(Value::Null), &RpcError::new(-32600, "invalid request")));
        };
        let params = request.get("params").cloned().unwrap_or(Value::Array(Vec::new()));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.call(method, &params, authenticated)
        }))
        .unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            eprintln!("tiger: panic in {method}: {message}");
            Err(RpcError::internal(format!("internal error: {message}")))
        });
        let id = id?;
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => Self::error_response(id, &error),
        })
    }

    fn error_response(id: Value, error: &RpcError) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() })
    }
}
