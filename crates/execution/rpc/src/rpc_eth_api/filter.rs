//! `eth_` RPC API for filtering.

use base_common_rpc_types::{Filter, FilterChanges, FilterId, PendingTransactionFilterKind};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// Rpc Interface for poll-based ethereum filter API.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait EthFilterApi {
    /// Creates a new filter and returns its id.
    #[method(name = "newFilter")]
    async fn new_filter(&self, filter: Filter) -> RpcResult<FilterId>;

    /// Creates a new block filter and returns its id.
    #[method(name = "newBlockFilter")]
    async fn new_block_filter(&self) -> RpcResult<FilterId>;

    /// Creates a pending transaction filter and returns its id.
    #[method(name = "newPendingTransactionFilter")]
    async fn new_pending_transaction_filter(
        &self,
        kind: Option<PendingTransactionFilterKind>,
    ) -> RpcResult<FilterId>;

    /// Returns all filter changes since last poll.
    #[method(name = "getFilterChanges")]
    async fn filter_changes(&self, id: FilterId) -> RpcResult<FilterChanges<base_common_rpc_types::BaseTransaction, base_common_rpc_types::Log>>;

    /// Returns all logs matching given filter (in a range 'from' - 'to').
    #[method(name = "getFilterLogs")]
    async fn filter_logs(&self, id: FilterId) -> RpcResult<Vec<base_common_rpc_types::Log>>;

    /// Uninstalls filter.
    #[method(name = "uninstallFilter")]
    async fn uninstall_filter(&self, id: FilterId) -> RpcResult<bool>;

    /// Returns logs matching given filter object.
    #[method(name = "getLogs")]
    async fn logs(&self, filter: Filter) -> RpcResult<Vec<base_common_rpc_types::Log>>;
}

/// Limits for logs queries
#[derive(Default, Debug, Clone, Copy)]
pub struct QueryLimits {
    /// Maximum number of blocks that could be scanned per filter
    pub max_blocks_per_filter: Option<u64>,
    /// Maximum number of logs that can be returned in a response
    pub max_logs_per_response: Option<usize>,
}

impl QueryLimits {
    /// Construct an object with no limits (more explicit than using default constructor)
    pub fn no_limits() -> Self {
        Default::default()
    }
}
