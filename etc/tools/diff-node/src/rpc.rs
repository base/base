//! The `fuzz_coverage` JSON-RPC method.

use base_diff_adapter::EdgeCounters;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{Deserialize, Serialize};

/// Edges hit since the previous reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCoverage {
    /// Instrumented edges in this binary.
    pub counters: usize,
    /// `[index, count]` for every edge hit since the previous reading, in index order. The count
    /// is the 8-bit counter, which wraps.
    pub hits: Vec<(usize, u8)>,
}

/// Coverage of the node's own request handling.
#[rpc(server, namespace = "fuzz")]
pub trait CoverageApi {
    /// Returns the edges hit since the previous call and zeroes the counters.
    #[method(name = "coverage")]
    fn coverage(&self) -> RpcResult<NodeCoverage>;
}

/// [`CoverageApiServer`] over the process's `SanitizerCoverage` counters.
#[derive(Debug, Clone, Copy)]
pub struct CoverageRpc;

impl CoverageApiServer for CoverageRpc {
    fn coverage(&self) -> RpcResult<NodeCoverage> {
        let hits = EdgeCounters::hits();
        EdgeCounters::reset();
        Ok(NodeCoverage { counters: EdgeCounters::len(), hits })
    }
}

/// Registers [`CoverageRpc`] on every configured RPC transport.
#[derive(Debug, Clone, Copy)]
pub struct CoverageRpcExtension;

impl BaseNodeExtension for CoverageRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(|ctx: &mut BaseRpcContext<'_>| {
            ctx.modules.merge_configured(CoverageRpc.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for CoverageRpcExtension {
    type Config = ();

    fn from_config((): ()) -> Self {
        Self
    }
}
