#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod transaction;

pub use transaction::TransactionConversionError;

mod block;
pub use block::RpcBlockConverter;

extern crate alloc;

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
