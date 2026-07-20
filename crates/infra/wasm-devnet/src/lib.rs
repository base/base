#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod devnet;
pub use devnet::Devnet;
#[cfg(target_family = "wasm")]
pub use devnet::{
    DEV_KEY, DapQueue, InMemoryDap, L1Block, L1ProviderError, L2ProviderError, SharedL1,
    WasmL1Provider, WasmL2Provider, address_from_verifying_key, l1_origin_from_attrs,
};

#[cfg(target_family = "wasm")]
mod wasm;
#[cfg(target_family = "wasm")]
pub use wasm::WasmDevnet;
