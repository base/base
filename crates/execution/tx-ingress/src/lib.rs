#![doc = include_str!("../README.md")]

mod config;
pub use config::TransactionIngressConfig;

mod extension;
pub use extension::TransactionIngressExtension;

mod protocol;
pub use protocol::{
    SubmitError, SubmitRequest, SubmitResponse, submit_response,
    transaction_ingress_service_client, transaction_ingress_service_server,
};

mod service;
pub use service::TransactionIngressService;
