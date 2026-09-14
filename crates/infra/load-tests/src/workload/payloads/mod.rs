//! Transaction payload types for different workload scenarios.

use alloy_primitives::Address;
use alloy_rpc_types::TransactionRequest;
use async_trait::async_trait;

use crate::{
    Result,
    workload::{SeededRng, chain_prep::ChainPrepContext},
};

mod transfer;
pub use transfer::TransferPayload;

mod calldata;
pub use calldata::CalldataPayload;

mod erc20;
pub use erc20::Erc20Payload;

mod storage;
pub use storage::StoragePayload;

mod double_counter;
pub use double_counter::{DOUBLE_COUNTER_GAS_LIMIT, DoubleCounterPayload};

mod precompile;
pub use precompile::{PrecompilePayload, parse_precompile_id};

mod looper;
pub use looper::PrecompileLooper;

mod uniswap;
pub use uniswap::UniswapV3Payload;

mod aerodrome;
pub use aerodrome::AerodromeClPayload;

mod b20;
pub use b20::B20TransferPayload;
pub(crate) use b20::{b20_salt_for, b20_token_for};

mod b20_lifecycle;

mod real_token_lifecycle;
pub use real_token_lifecycle::recover_real_tokens;

mod osaka;
pub use osaka::OsakaPayload;

/// A transaction payload generator with optional chain preparation.
#[async_trait]
pub trait Payload: Send + Sync + std::fmt::Debug {
    /// Returns the name of this payload type.
    fn name(&self) -> &'static str;

    /// Returns true when this payload uses the runner-supplied recipient address.
    fn uses_runner_recipient(&self) -> bool;

    /// Returns true when the runner recipient should be this sender's pair partner (alice <-> bob)
    /// rather than the next sender in a ring.
    fn uses_pair_recipient(&self) -> bool {
        false
    }

    /// Generates a transaction request.
    fn generate(&self, rng: &mut SeededRng, from: Address, to: Address) -> TransactionRequest;

    /// Optional chain preparation before the measured load phase. Default: no-op.
    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Optional cleanup after the load phase. Default: no-op.
    async fn teardown(&self, ctx: &ChainPrepContext<'_>) -> Result<()> {
        let _ = ctx;
        Ok(())
    }
}
