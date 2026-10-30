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
    DoubleCounterPayload, EIP8430_TRANSFER_GAS_LIMIT, Eip8430TransferPayload, Erc20Payload,
    LiquidityOps, OsakaPayload, Payload, PrecompileLooper, PrecompilePayload, SenderPosition,
    StoragePayload, TokenMoveContext, TokenMovePayload, TransferPayload, USDC, UniswapPositions,
    UniswapV3Hop, UniswapV3LiquidityPayload, UniswapV3Payload, X402Context, X402Payload,
    encode_erc20_approve, recover_real_tokens,
};
pub(crate) use payloads::{DISPERSE, b20_salt_for, b20_token_for};

mod generator;
pub use generator::WorkloadGenerator;
