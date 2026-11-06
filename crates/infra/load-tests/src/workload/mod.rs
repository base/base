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
    AaBundlePayload, AerodromeClPayload, B20TransferPayload, CalldataPayload,
    DEFAULT_REVERT_ARB_GAS_LIMIT, DOUBLE_COUNTER_GAS_LIMIT, DoubleCounterPayload,
    EIP8430_TRANSFER_GAS_LIMIT, Eip8430TransferPayload, Erc20Payload, LiquidityOps, OsakaPayload,
    Payload, PrecompileLooper, PrecompilePayload, REVERT_ARB_SELECTOR, RevertArbPayload,
    SenderPosition, StoragePayload, TokenMoveContext, TokenMovePayload, TransferPayload, USDC,
    UniswapPositions, UniswapV3Hop, UniswapV3LiquidityPayload, UniswapV3Payload, X402Context,
    X402Payload, encode_erc20_approve, recover_real_tokens,
};
pub(crate) use payloads::{DISPERSE, MAX_TOKEN_MOVE_TRANSFERS_PER_TX, b20_salt_for, b20_token_for};

mod generator;
pub(crate) use generator::SelectedPayload;
pub use generator::WorkloadGenerator;
