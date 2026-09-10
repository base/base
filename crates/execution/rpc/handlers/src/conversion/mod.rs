//! Request conversion, fee handling, simulation, and state overrides.

mod transaction;

pub use transaction::{ConvertReceiptInput, TransactionConversionError};

mod block;
pub use block::RpcBlockConverter;

mod fees;
pub use fees::{CallFees, CallFeesError};
mod transaction_env;
pub use transaction_env::{EthTxEnvError, TryIntoTxEnv};
mod base_transaction_env;
mod eip8130;
pub use eip8130::{
    AUTHENTICATOR_SELECTOR_LEN, Eip8130TransactionConverter, MAX_AUTH_SIZE, STUB_AUTH_FILL,
};
mod overrides;
pub use overrides::{
    OverrideBlockHashes, StateOverrideError, apply_block_overrides, apply_state_overrides,
};

mod call;
pub use call::{CallError, InsufficientFundsError, caller_gas_allowance};
