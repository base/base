#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod balance;
pub use balance::BalanceLayout;

mod config;
pub use config::{
    FeedConfig, LegConfig, PayerConfig, PayerConfigError, PayerTerms, PriceConfig, QuoteConfig,
    TokenConfig,
};

mod error;
pub use error::{GasDiagnostic, PayerErrorCode, PayerRejection, Requote, Revert, Shortfall};

mod ingress;
#[cfg(test)]
pub use ingress::MockValidityIngress;
pub use ingress::ValidityIngress;

mod payment;
pub use payment::{IERC20, TokenPayment, TransferOutcome};

mod rpc;
pub use rpc::PayerApiServer;

mod service;
pub use service::PayerService;

mod token;
pub use token::PayerToken;

mod types;
pub use types::{
    GasEstimate, GetTermsParams, GetTermsResult, OfferConditions, PaymentOption,
    SendTransactionParams, SendTransactionResult, TokenCharged, TokenChoice, TokenPaymentOffer,
};
