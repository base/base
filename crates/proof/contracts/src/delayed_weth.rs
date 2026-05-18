//! `DelayedWETH` contract bindings.
//!
//! Used to read the withdrawal delay and pending unlocks from the
//! `DelayedWETH` contract. The bond lifecycle in `AggregateVerifier`
//! requires waiting `delay()` between the first `claimCredit()` call
//! (which triggers `unlock()`) and the second call (which triggers
//! `withdraw()`).

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::RootProvider;
use async_trait::async_trait;

use crate::ContractError;

alloy_sol_types::sol! {
    /// `DelayedWETH` contract interface (read-only subset).
    #[sol(rpc)]
    interface IDelayedWETH {
        /// Returns the withdrawal delay in seconds.
        function delay() external view returns (uint256);

        /// Returns `(amount, timestamp)` for `withdrawals[holder][recipient]`.
        /// Both fields are zero when no unlock has been recorded.
        function withdrawals(address holder, address recipient)
            external
            view
            returns (uint256 amount, uint256 timestamp);
    }
}

/// Async trait for querying the `DelayedWETH` contract.
#[async_trait]
pub trait DelayedWETHClient: Send + Sync {
    /// Returns the withdrawal delay enforced between `unlock()` and `withdraw()`.
    async fn delay(&self) -> Result<Duration, ContractError>;

    /// Returns `(amount, timestamp)` for `withdrawals[holder][recipient]`.
    /// Both fields are zero when no unlock has been recorded.
    ///
    /// `holder` is the address that called `unlock()` (the dispute
    /// game proxy in our use case); `recipient` is the sub-account
    /// the unlock was scoped to (the on-chain `bondRecipient`).
    async fn withdrawals(
        &self,
        holder: Address,
        recipient: Address,
    ) -> Result<(U256, u64), ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct DelayedWETHContractClient {
    contract: IDelayedWETH::IDelayedWETHInstance<RootProvider>,
}

impl DelayedWETHContractClient {
    /// Creates a new client for the `DelayedWETH` contract at the given address.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Result<Self, ContractError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IDelayedWETH::IDelayedWETHInstance::new(address, provider);
        Ok(Self { contract })
    }
}

#[async_trait]
impl DelayedWETHClient for DelayedWETHContractClient {
    async fn delay(&self) -> Result<Duration, ContractError> {
        let delay_u256: U256 = self
            .contract
            .delay()
            .call()
            .await
            .map_err(|e| ContractError::Call { context: "delay failed".into(), source: e })?;

        let delay_secs: u64 = delay_u256
            .try_into()
            .map_err(|_| ContractError::Validation("delay overflows u64".into()))?;

        Ok(Duration::from_secs(delay_secs))
    }

    async fn withdrawals(
        &self,
        holder: Address,
        recipient: Address,
    ) -> Result<(U256, u64), ContractError> {
        let result =
            self.contract.withdrawals(holder, recipient).call().await.map_err(|e| {
                ContractError::Call { context: "withdrawals failed".into(), source: e }
            })?;

        let timestamp: u64 = result
            .timestamp
            .try_into()
            .map_err(|_| ContractError::Validation("withdrawals timestamp overflows u64".into()))?;

        Ok((result.amount, timestamp))
    }
}
