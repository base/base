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
pub use erc4337::{PackedUserOperation, UserOperation};

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

mod trace_parity;
pub use trace_parity::*;

mod trace_tracerequest;
pub use trace_tracerequest::*;

mod u256_numeric_string;

mod rpc_modules;
pub use rpc_modules::RpcModules;

mod response;
pub use response::{
    BlockResponse, HeaderResponse, ReceiptResponse, TransactionFailedError, TransactionResponse,
};

mod block_transactions;
pub use block_transactions::{BlockTransactionHashes, BlockTransactions, BlockTransactionsKind};

mod transaction_builders;
pub use transaction_builders::{TransactionBuilder4844, TransactionBuilder7702};

mod inclusion;
pub use inclusion::InclusionInfo;

mod consensus_peers;
pub use consensus_peers::{
    Connectedness, ConsensusPeerInfo, Direction, GossipScores, PeerCount, PeerDump, PeerScores,
    PeerStats, ReqRespScores, TopicScores,
};

mod block_info;

mod rollup_sync;
pub use rollup_sync::RollupSyncStatus;

mod safe_head;
pub use safe_head::SafeHeadResponse;

mod rollup_output;
pub use rollup_output::OutputResponse;

mod health;
pub use health::HealthzResponse;

mod conductor;
pub use conductor::{ClusterMembership, ServerInfo, ServerSuffrage, UnknownServerSuffrage};

mod gas_price_config;
pub use gas_price_config::{
    DEFAULT_GAS_PRICE_BLOCKS, DEFAULT_GAS_PRICE_PERCENTILE, DEFAULT_IGNORE_GAS_PRICE,
    DEFAULT_MAX_GAS_PRICE, GasPriceOracleConfig, MAX_HEADER_HISTORY, MAX_REWARD_PERCENTILE_COUNT,
};

mod pending_block_kind;
pub use pending_block_kind::PendingBlockKind;

mod rpc_defaults;
pub use rpc_defaults::*;
