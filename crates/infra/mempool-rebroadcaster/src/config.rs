/// Configuration for the [`Rebroadcaster`](crate::Rebroadcaster) service.
#[derive(Debug, Clone)]
pub struct RebroadcasterConfig {
    /// HTTP endpoint for the geth mempool node.
    pub geth_mempool_endpoint: String,
    /// HTTP endpoint for the reth mempool node.
    pub reth_mempool_endpoint: String,
}
