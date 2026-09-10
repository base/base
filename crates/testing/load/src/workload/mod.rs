//! Workload generation, account management, and transaction payloads.

mod accounts;
pub use accounts::{AccountPool, FundedAccount};

mod seeded;
pub use seeded::SeededRng;

mod key_stream;
pub use key_stream::KeyStream;

mod chain_prep;
pub use chain_prep::{
    ChainPrepContext, ChainPrepOutputs, RealTokenAcquisition, RealTokenPairTokenSetup,
    RealTokenRecoverySummary, RealTokenSetup,
};
pub(crate) use chain_prep::{PREP_CONCURRENCY, await_token_balances, encode_erc20_balance_of};

mod payloads;
pub use payloads::{
    AerodromeClPayload, B20TransferPayload, CalldataPayload, DOUBLE_COUNTER_GAS_LIMIT,
    DoubleCounterPayload, Erc20Payload, OsakaPayload, Payload, PrecompileLooper, PrecompilePayload,
    StoragePayload, TransferPayload, UniswapV3Payload, parse_precompile_id, recover_real_tokens,
};
pub(crate) use payloads::{b20_salt_for, b20_token_for};

mod generator;
pub use generator::WorkloadGenerator;
