#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod receipt;
pub use receipt::{OpDepositReceipt, OpDepositReceiptWithBloom, OpReceiptEnvelope, OpTxReceipt};

mod transaction;
pub use transaction::{
    DepositSourceDomain, DepositSourceDomainIdentifier, L1InfoDepositSource, OpTxEnvelope,
    OpTxType, OpTypedTransaction, TxDeposit, UpgradeDepositSource, UserDepositSource,
    DEPOSIT_TX_TYPE_ID,
};

pub mod hardforks;
pub use hardforks::Hardforks;

mod block;
pub use block::OpBlock;
