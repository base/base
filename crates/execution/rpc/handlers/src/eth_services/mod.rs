//! Ethereum RPC caches, conversion, errors, and request services.

// `url` is needed for serde support on `reqwest::Url`
use url as _;

pub mod block;
pub mod builder;
pub mod cache;
pub mod capabilities;
pub mod error;
pub mod fee_history;
pub mod gas_oracle;
pub mod id_provider;
pub mod logs_utils;
pub mod pending_block;
pub mod receipt;
pub mod simulate;
pub mod transaction;
pub mod tx_forward;
pub mod utils;

pub use base_common_types_rpc::FillTransaction;
pub use block::CachedTransaction;
pub use builder::config::{EthConfig, EthFilterConfig};
pub use cache::{
    EthStateCache, config::EthStateCacheConfig, db::StateCacheDb,
    multi_consumer::MultiConsumerLruCache,
};
pub use capabilities::{EthCapabilities, EthCapabilitiesHead, EthCapabilitiesResource};
pub use error::{EthApiError, EthResult, RevertError, RpcInvalidTransactionError, SignError};
pub use fee_history::{FeeHistoryCache, FeeHistoryCacheConfig, FeeHistoryEntry};
pub use gas_oracle::{GasCap, GasPriceOracle, GasPriceOracleResult, RPC_DEFAULT_GAS_CAP};
pub use id_provider::EthSubscriptionIdProvider;
pub use pending_block::{PendingBlock, PendingBlockEnv, PendingBlockEnvOrigin};
pub use transaction::TransactionSource;
pub use tx_forward::ForwardConfig;

mod base_error;
pub use base_error::{BaseEthApiError, BaseInvalidTransactionError, SequencerClientError};

mod base_receipt;
pub use base_receipt::{BaseReceiptBuilder, BaseReceiptConverter, ReceiptFieldsBuilder};
mod base_time;
pub use base_time::BaseTimeCache;
mod base_tx_info;
pub use base_tx_info::BaseTxInfoMapper;
mod base_rpc_converter;
pub use base_rpc_converter::BaseRpcConverter;

pub use base_common_types_rpc::GasPriceOracleConfig;
