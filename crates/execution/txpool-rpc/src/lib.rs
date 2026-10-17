#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod candidates;
pub use candidates::{
    AdminCandidateSelectionApiImpl, AdminCandidateSelectionApiServer, CandidateSiblings,
    CandidateSubmissionResponse, SendRawTransactionCandidatesApiImpl,
    SendRawTransactionCandidatesApiServer, SendRawTransactionCandidatesOptions,
};

mod rpc;
pub use rpc::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, BlockCadence, SendRawTransactionValidityApiImpl,
    SendRawTransactionValidityApiServer, SendRawTransactionValidityOptions, Status,
    TransactionStatusApiImpl, TransactionStatusApiServer, TransactionStatusResponse,
    VALIDITY_TX_PRE_EVEREST_RPC_ERROR,
};

mod extension;
pub use extension::{
    DEFAULT_MAX_VALIDITY_EXPIRY_SECS, DEFAULT_MAX_VALIDITY_PREDICATES,
    SendRawTransactionCandidatesConfig, SendRawTransactionCandidatesExtension,
    SendRawTransactionValidityConfig, SendRawTransactionValidityExtension, TxPoolRpcConfig,
    TxPoolRpcExtension, ValiditySignatureMode,
};
