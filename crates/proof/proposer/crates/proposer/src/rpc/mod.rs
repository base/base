//! RPC client implementations for L1, L2, and Rollup nodes.

use alloy::primitives::B256;
use async_trait::async_trait;
use eyre::Result;
use url::Url;

/// L1 RPC client for interacting with Ethereum.
#[derive(Debug)]
pub struct L1Client {
    endpoint: Url,
}

impl L1Client {
    /// Creates a new L1 client.
    pub const fn new(endpoint: Url) -> Self {
        Self { endpoint }
    }

    /// Returns the endpoint URL.
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }
}

/// L2 RPC client for interacting with the OP Stack chain.
#[derive(Debug)]
pub struct L2Client {
    endpoint: Url,
    is_reth: bool,
}

impl L2Client {
    /// Creates a new L2 client.
    pub const fn new(endpoint: Url, is_reth: bool) -> Self {
        Self { endpoint, is_reth }
    }

    /// Returns the endpoint URL.
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns whether this client uses reth-specific methods.
    pub const fn is_reth(&self) -> bool {
        self.is_reth
    }
}

/// Rollup RPC client for interacting with the OP Stack rollup node.
#[derive(Debug)]
pub struct RollupClient {
    endpoint: Url,
}

impl RollupClient {
    /// Creates a new rollup client.
    pub const fn new(endpoint: Url) -> Self {
        Self { endpoint }
    }

    /// Returns the endpoint URL.
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }
}

/// Trait for block data providers.
#[async_trait]
pub trait BlockProvider {
    /// Gets the latest block number.
    async fn get_block_number(&self) -> Result<u64>;

    /// Gets a block hash by number.
    async fn get_block_hash(&self, number: u64) -> Result<Option<B256>>;
}

/// Trait for output root providers.
#[async_trait]
pub trait OutputRootProvider {
    /// Gets the output root at a given block number.
    async fn get_output_root(&self, block_number: u64) -> Result<B256>;
}
