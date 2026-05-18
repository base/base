//! Per-game [`DelayedWETHClient`] resolver.
//!
//! Each game's `DELAYED_WETH` contract address is a CWIA-immutable
//! parameter read from the game proxy. The resolver hides that lookup
//! plus the client construction so the bond worker stays decoupled from
//! the concrete contracts crate.

use std::sync::Arc;

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_contracts::{
    AggregateVerifierClient, ContractError, DelayedWETHClient, DelayedWETHContractClient,
};
use derive_more::Debug;
use url::Url;

/// Resolves the [`DelayedWETHClient`] for a given dispute game.
#[async_trait]
pub trait DelayedWETHResolver: Send + Sync {
    /// Returns a client for the `DelayedWETH` contract attached to `game`.
    async fn resolve(&self, game: Address) -> Result<Arc<dyn DelayedWETHClient>, ContractError>;
}

/// Production [`DelayedWETHResolver`] that reads `DELAYED_WETH` from the
/// game contract and builds a fresh [`DelayedWETHContractClient`].
#[derive(Debug)]
pub struct L1DelayedWETHResolver {
    /// Aggregate verifier client used to look up the per-game `DELAYED_WETH` address.
    #[debug(skip)]
    verifier: Arc<dyn AggregateVerifierClient>,
    /// L1 RPC endpoint passed to every spawned [`DelayedWETHContractClient`].
    l1_rpc_url: Url,
}

impl L1DelayedWETHResolver {
    /// Builds a resolver against the given verifier client and L1 RPC URL.
    pub fn new(verifier: Arc<dyn AggregateVerifierClient>, l1_rpc_url: Url) -> Self {
        Self { verifier, l1_rpc_url }
    }
}

#[async_trait]
impl DelayedWETHResolver for L1DelayedWETHResolver {
    async fn resolve(&self, game: Address) -> Result<Arc<dyn DelayedWETHClient>, ContractError> {
        let address = self.verifier.delayed_weth(game).await?;
        let client = DelayedWETHContractClient::new(address, self.l1_rpc_url.clone())?;
        Ok(Arc::new(client))
    }
}
