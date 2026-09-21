#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{
    DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY, DEFAULT_INLINE_SIMULATION_TIMEOUT_MS,
    DEFAULT_INLINE_SIMULATION_WORKERS, DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS,
    DEFAULT_RESEND_AFTER_MS, TxForwardingConfig,
};

mod extension;
pub use extension::TxForwardingExtension;

mod forwarder;
pub use forwarder::{ForwardRequest, InsertValidatedTransaction};

mod reader;

mod service;
pub use service::{ForwardingSetupError, ShutdownReport, TxForwardingHandle, TxForwardingService};
