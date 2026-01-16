#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Re-exports from `reth-rpc-eth-types`.
pub mod rpc {
    pub use reth_rpc_eth_types::{EthApiError, SignError};
}

/// Re-exports from `reth-optimism-evm`.
pub mod optimism {
    pub use reth_optimism_evm::extract_l1_info_from_tx;
}

// Re-export at the crate root for convenience
pub use optimism::extract_l1_info_from_tx;
pub use rpc::{EthApiError, SignError};
