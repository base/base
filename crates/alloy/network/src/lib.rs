#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use alloy_network::{
    BlockResponse, ReceiptResponse, TransactionBuilder, TransactionResponse, TxSignerSync, eip2718,
};

mod base;
pub use base::Base;

mod builder;

mod wallet;

#[cfg(feature = "reth")]
mod reth;
