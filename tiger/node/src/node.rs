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
    /// The code of a state read whose block a reorg dropped after the request resolved it, as
    /// `latest` can name the old head while a forkchoice update replaces it. [`Node::rpc`]
    /// answers such a request again, so the read sees the old or the new head as base-reth's
    /// does, and never sends this code.
    pub const REORGED: i64 = i64::MIN;

    /// An error with `code` and `message`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    /// `-32602`.
    pub fn invalid_params(message: String) -> Self {
        Self::new(-32602, message)
    }

    /// jsonrpsee's `-32601` for `method`: `Method not found`, which does not name it.
    pub fn method_not_found(_method: &str) -> Self {
        Self::new(-32601, "Method not found")
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
    /// Witnesses of recent blocks for peers, when the node serves them (feature `tp2p`).
    #[cfg(feature = "tp2p")]
    pub witness_cache: Option<crate::WitnessCache>,
    /// tp2p, once started (feature `tp2p`).
    #[cfg(feature = "tp2p")]
    pub tp2p: std::sync::Arc<OnceLock<std::sync::Arc<crate::Tp2p>>>,
}

impl Node {
    /// Opens the node's datadir. With `upstream`, a new datadir starts stateless at the
    /// upstream's block `start` (a number or tag), and missing state is read from the upstream.
    pub fn open(datadir: &Path, genesis: Genesis, upstream: Option<(crate::Upstream, String)>) -> Result<Self, String> {
        let spec = genesis.chain.clone();
        let stateless = upstream.as_ref().map(|(upstream, block)| crate::StatelessStart { upstream, block });
        let chain = Chain::open(datadir, genesis, stateless)?;
        #[cfg(feature = "tp2p")]
        let source = crate::WitnessSource { upstream: upstream.as_ref().map(|(upstream, _)| upstream.clone()), ..Default::default() };
        Ok(Self {
            #[cfg(feature = "tp2p")]
            tp2p: std::sync::Arc::clone(&source.tp2p),
            spec,
            chain: RwLock::new(chain),
            engine: Mutex::new(EngineState::default()),
            sync: OnceLock::new(),
            filters: crate::RpcFilters::default(),
            rpc_config: crate::RpcConfig::default(),
            prewarm: OnceLock::new(),
            persistence: crate::Persistence::default(),
            #[cfg(feature = "tp2p")]
            witness_cache: None,
            #[cfg(feature = "tp2p")]
            witnesses: upstream.is_some().then(|| crate::Witnesses::new(&source)),
            #[cfg(not(feature = "tp2p"))]
            witnesses: upstream.as_ref().map(|(upstream, _)| crate::Witnesses::new(upstream)),
            upstream: upstream.map(|(upstream, _)| upstream),
        })
    }

    /// Answers one JSON-RPC call. `authenticated` calls (the auth port) may use `engine_*`.
    pub fn call(&self, method: &str, params: &Value, authenticated: bool) -> Result<Value, RpcError> {
        if !method.starts_with("engine_") {
            self.rpc(method, params)
        } else if authenticated {
            self.engine(method, params)
        } else {
            Err(RpcError::method_not_found(method))
        }
    }
}
