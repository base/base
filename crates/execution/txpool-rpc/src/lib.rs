#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod rpc;
pub use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;
pub use rpc::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, SendRawTransactionValidityApiImpl,
    SendRawTransactionValidityApiServer, SendRawTransactionValidityOptions, Status,
    TransactionStatusApiImpl, TransactionStatusApiServer, TransactionStatusResponse,
    VALIDITY_TX_PRE_ZENITH_RPC_ERROR,
};

mod builder_config;
pub use builder_config::BuilderApiConfig;
mod shadow_validity;
pub use shadow_validity::{
    InjectionOutcome, MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS, ShadowValidityBuilderApi,
    ShadowValidityConfig, ShadowValidityConfigError,
};
mod metrics;
pub use metrics::ValidityMetrics;
