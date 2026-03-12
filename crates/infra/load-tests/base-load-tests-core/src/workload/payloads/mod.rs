use alloy_primitives::Address;

use crate::{rpc::TransactionRequest, workload::SeededRng};

mod transfer;
pub use transfer::TransferPayload;

mod calldata;
pub use calldata::CalldataPayload;

mod erc20;
pub use erc20::Erc20Payload;

mod storage;
pub use storage::StoragePayload;

mod precompile;
pub use precompile::{PrecompilePayload, PrecompileTarget};

mod uniswap;
pub use uniswap::{UniswapV2Payload, UniswapV3Payload};

/// A transaction payload generator.
pub trait Payload: Send + Sync + std::fmt::Debug {
    /// Returns the name of this payload type.
    fn name(&self) -> &'static str;

    /// Generates a transaction request.
    fn generate(&self, rng: &mut SeededRng, from: Address, to: Address) -> TransactionRequest;
}
