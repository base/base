#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
extern crate alloc;

mod genesis;
pub use genesis::{ChainInfo, FeeInfo, GenesisInfo, UpgradeInfo};

mod base_receipt;
pub use base_receipt::{
    BaseTransactionReceipt, Eip8130ReceiptFields, L1BlockInfo, TransactionReceiptFields,
};

mod eip8130;
pub use eip8130::{EIP8130_PRE_ZENITH_RPC_ERROR, Eip8130Nonce};

mod base_transaction;
pub use alloy_eips::eip4895::{Withdrawal, Withdrawals};
pub use alloy_network_primitives::{
    BlockTransactionHashes, BlockTransactions, BlockTransactionsKind,
};
pub use base_transaction::{
    BaseTransaction, BaseTransactionFields, BaseTransactionRequest, Eip8130AuthScheme,
    Eip8130RequestFields,
};

mod account;
pub use account::*;

mod block;
pub use block::*;

mod call;
pub use call::{Bundle, EthCallResponse, StateContext, TransactionIndex};

pub mod error;

mod fee;
pub use fee::{FeeHistory, TxGasAndReward};

mod filter;
pub use filter::*;

mod index;
pub use index::Index;

mod log;
pub use log::*;

pub mod pubsub;

mod raw_log;
pub use raw_log::{Log as RawLog, logs_bloom};

pub mod state;

mod syncing;
pub use syncing::*;

pub mod transaction;
pub use transaction::*;

mod work;
pub use work::Work;

/// This module provides implementations for ERC-4337.
pub mod erc4337;
pub use erc4337::{
    PackedUserOperation, SendUserOperation, SendUserOperationResponse, UserOperation,
    UserOperationGasEstimation, UserOperationReceipt,
};

pub mod simulate;

mod base_transactions;

mod txpool;
pub use txpool::{
    TxpoolContent, TxpoolContentFrom, TxpoolInspect, TxpoolInspectSummary, TxpoolStatus,
};

mod trace_common;
pub use trace_common::*;

mod trace_filter;
pub use trace_filter::*;

mod trace_geth;
pub use trace_geth::*;

mod trace_opcode;
pub use trace_opcode::*;

mod trace_otterscan;
pub use trace_otterscan::*;

mod trace_parity;
pub use trace_parity::*;

mod trace_tracerequest;
pub use trace_tracerequest::*;

mod bundle_call;
pub use bundle_call::*;

mod bundle_common;
pub use bundle_common::*;

mod bundle_sim;
pub use bundle_sim::*;

mod u256_numeric_string;
