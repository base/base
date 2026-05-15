//! Workload generation, account management, and transaction payloads.

mod accounts;
pub use accounts::{AccountPool, FundedAccount};

mod seeded;
pub use seeded::SeededRng;

mod payloads;
pub(crate) const SIMULATOR_STORAGE_CHUNK_SIZE: u64 = 100;
pub(crate) const SIMULATOR_ACCOUNT_CHUNK_SIZE: u64 = 100;
pub use payloads::{
    AerodromeClPayload, B20TransferPayload, CalldataPayload, Erc20Payload, OsakaPayload, Payload,
    PrecompileLooper, PrecompilePayload, SimulatorPayload, StoragePayload, TransferPayload,
    UniswapV3Payload, parse_precompile_id,
};

mod generator;
pub use generator::WorkloadGenerator;
