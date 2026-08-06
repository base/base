//! Chain preparation context and shared helpers for payload `prepare` / `teardown`.

use std::time::{Duration, Instant};

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{trace, warn};
use url::Url;

use crate::{
    BaselineError, Result,
    rpc::{QueryProvider, RpcResultExt},
    workload::AccountPool,
};

/// Concurrent RPC requests during chain-prep operations (matches funding concurrency).
pub(crate) const PREP_CONCURRENCY: usize = 32;

/// Multiplier applied to base fee when deriving prep/submission max fees.
const PREP_MAX_FEE_BASE_FEE_MULTIPLIER: u128 = 4;

/// Real-token setup executed before measured swap workloads.
#[derive(Debug, Clone)]
pub struct RealTokenSetup {
    /// Whether chain ID 8453 is allowed for this setup.
    pub allow_chain_id_8453: bool,
    /// WETH contract address.
    pub weth: Address,
    /// Target WETH balance to leave each sender with after setup.
    pub weth_amount_per_sender: U256,
    /// Non-WETH token setup for bidirectional swap parity.
    pub pair_token: RealTokenPairTokenSetup,
    /// Allowance amount to approve for each measured router.
    pub approval_amount: U256,
}

/// Summary of real-token balances recovered before native ETH drain.
#[derive(Debug, Clone, Default)]
pub struct RealTokenRecoverySummary {
    /// Pair-token raw units swapped back into WETH.
    pub pair_token_swapped: U256,
    /// WETH unwrapped back into native ETH.
    pub weth_unwrapped: U256,
}

/// Non-WETH side of the real-token pair.
#[derive(Debug, Clone)]
pub struct RealTokenPairTokenSetup {
    /// Pair token contract address.
    pub token: Address,
    /// Target pair-token balance per sender.
    pub amount_per_sender: U256,
    /// How to acquire pair-token balances during setup.
    pub acquisition: RealTokenAcquisition,
}

/// Explicit setup route for acquiring the pair token.
#[derive(Debug, Clone)]
pub enum RealTokenAcquisition {
    /// Uniswap V3 `exactInputSingle` route.
    UniswapV3ExactInput {
        /// Router contract address.
        router: Address,
        /// Fee tier.
        fee: u32,
        /// WETH input amount per sender.
        amount_in: U256,
        /// Minimum pair-token output amount.
        min_amount_out: U256,
    },
    /// Aerodrome Slipstream `exactInputSingle` route.
    AerodromeClExactInput {
        /// Router contract address.
        router: Address,
        /// Tick spacing.
        tick_spacing: i32,
        /// WETH input amount per sender.
        amount_in: U256,
        /// Minimum pair-token output amount.
        min_amount_out: U256,
    },
}

impl RealTokenAcquisition {
    /// Returns the router used by this setup route.
    pub const fn router(&self) -> Address {
        match self {
            Self::UniswapV3ExactInput { router, .. }
            | Self::AerodromeClExactInput { router, .. } => *router,
        }
    }

    /// Returns the input amount consumed by this setup route.
    pub const fn amount_in(&self) -> U256 {
        match self {
            Self::UniswapV3ExactInput { amount_in, .. }
            | Self::AerodromeClExactInput { amount_in, .. } => *amount_in,
        }
    }

    /// Returns the minimum output amount expected by this setup route.
    pub const fn min_amount_out(&self) -> U256 {
        match self {
            Self::UniswapV3ExactInput { min_amount_out, .. }
            | Self::AerodromeClExactInput { min_amount_out, .. } => *min_amount_out,
        }
    }
}

/// Mutable outputs produced by payload chain preparation.
#[derive(Debug, Clone, Default)]
pub struct ChainPrepOutputs {
    /// Per-run salt for deriving each sender's own B-20 token.
    pub b20_run_salt: Option<alloy_primitives::B256>,
    /// Whether real-token balances were prepared (at most once per run).
    pub real_tokens_prepared: bool,
    /// Whether sender nonces/balances should be refreshed after prepare.
    pub needs_sender_refresh: bool,
}

/// Context passed to [`crate::workload::Payload::prepare`] / `teardown`.
///
/// Holds RPC/account handles and prep inputs without any load-pacing knowledge.
#[derive(Debug)]
pub struct ChainPrepContext<'a> {
    /// Query provider for `eth_call` / balance checks.
    pub client: &'a QueryProvider,
    /// Funded sender accounts used for prep transactions.
    pub accounts: &'a AccountPool,
    /// Chain ID for prep transactions.
    pub chain_id: u64,
    /// Cap on max fee per gas.
    pub max_gas_price: u128,
    /// Primary RPC used to submit prep transactions.
    pub primary_submission_rpc: Url,
    /// When true, progress bars are hidden (live display mode).
    pub hide_progress: bool,
    /// Concurrent prep RPC operations.
    pub concurrency: usize,
    /// B-20 mint amount per sender (when B-20 prep runs).
    pub b20_mint: U256,
    /// Optional real-token setup for swap payloads.
    pub real_token_setup: Option<&'a RealTokenSetup>,
    /// Measured-swap router addresses (for approvals).
    pub swap_routers: Vec<Address>,
    /// Outputs written by prepare hooks.
    pub outputs: ChainPrepOutputs,
}

impl ChainPrepContext<'_> {
    /// Creates a progress bar for prep phases (or a hidden bar in live-display mode).
    pub fn progress_bar(&self, total: u64, prefix: &str) -> ProgressBar {
        if self.hide_progress {
            return ProgressBar::hidden();
        }
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template("{prefix} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                .expect("valid template")
                .progress_chars("█▓░"),
        );
        pb.set_prefix(prefix.to_string());
        pb
    }
}

/// Derives a prep/submission max fee from base fee, tip, and cap.
pub(crate) fn prep_submission_max_fee(
    base_fee: u128,
    priority_fee: u128,
    max_gas_price: u128,
) -> u128 {
    let target = base_fee
        .saturating_mul(PREP_MAX_FEE_BASE_FEE_MULTIPLIER)
        .max(base_fee.saturating_add(priority_fee));
    target.min(max_gas_price).max(priority_fee)
}

/// Encodes an ERC-20 `balanceOf(address)` call.
pub(crate) fn encode_erc20_balance_of(account: Address) -> Bytes {
    sol! {
        function balanceOf(address account) external view returns (uint256);
    }
    Bytes::from(balanceOfCall { account }.abi_encode())
}

/// Waits for token balances to reach a target after mint/distribution transactions.
pub(crate) async fn await_token_balances(
    client: &QueryProvider,
    pending_accounts: &mut Vec<(Address, Address)>,
    target_balance: U256,
    pb: &ProgressBar,
) -> Result<usize> {
    let timeout = Duration::from_secs(60);
    let poll_interval = Duration::from_millis(500);
    let start = Instant::now();
    let mut settled = 0usize;

    while !pending_accounts.is_empty() && start.elapsed() < timeout {
        tokio::time::sleep(poll_interval).await;

        let mut still_pending = Vec::new();
        for (token, sender) in pending_accounts.drain(..) {
            let call_data = encode_erc20_balance_of(sender);
            match client
                .call(TransactionRequest::default().with_to(token).with_input(call_data).into())
                .await
                .rpc("eth_call")
            {
                Ok(bytes) if U256::from_be_slice(bytes.as_ref()) >= target_balance => {
                    trace!(token = %token, sender = %sender, "token balance settled");
                    settled += 1;
                    pb.inc(1);
                }
                Ok(_) => {
                    still_pending.push((token, sender));
                }
                Err(e) => {
                    warn!(
                        token = %token,
                        sender = %sender,
                        error = %e,
                        "failed to check token balance"
                    );
                    still_pending.push((token, sender));
                }
            }
        }
        *pending_accounts = still_pending;
    }

    if !pending_accounts.is_empty() {
        let sample: Vec<_> = pending_accounts.iter().take(3).copied().collect();
        return Err(BaselineError::Transaction(format!(
            "{} token balances did not reach target within timeout; sample: {sample:?}",
            pending_accounts.len(),
        )));
    }

    Ok(settled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prep_submission_max_fee_covers_base_fee_plus_tip() {
        let base_fee = 1_000_000u128;
        let priority_fee = 100u128;
        let cap = 10_000_000_000u128;
        let max_fee = prep_submission_max_fee(base_fee, priority_fee, cap);
        assert!(max_fee >= base_fee + priority_fee);
        assert_eq!(max_fee, base_fee * PREP_MAX_FEE_BASE_FEE_MULTIPLIER);
    }

    #[test]
    fn default_prepare_outputs_are_empty() {
        let outputs = ChainPrepOutputs::default();
        assert!(outputs.b20_run_salt.is_none());
        assert!(!outputs.real_tokens_prepared);
        assert!(!outputs.needs_sender_refresh);
    }
}
