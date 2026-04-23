//! Transaction payload types for different workload scenarios.

use alloy_primitives::Address;
use alloy_rpc_types::TransactionRequest;

use crate::workload::SeededRng;

mod transfer;
pub use transfer::TransferPayload;

mod calldata;
pub use calldata::CalldataPayload;

mod erc20;
pub use erc20::Erc20Payload;

mod storage;
pub use storage::StoragePayload;

mod precompile;
pub use precompile::{PrecompilePayload, parse_precompile_id};

mod looper;
pub use looper::PrecompileLooper;

mod uniswap;
pub use uniswap::UniswapV3Payload;

mod aerodrome;
pub use aerodrome::AerodromeClPayload;

mod osaka;
pub use osaka::OsakaPayload;

/// A transaction payload generator.
pub trait Payload: Send + Sync + std::fmt::Debug {
    /// Returns the name of this payload type.
    fn name(&self) -> &'static str;

    /// Generates a transaction request.
    fn generate(&self, rng: &mut SeededRng, from: Address, to: Address) -> TransactionRequest;
}
