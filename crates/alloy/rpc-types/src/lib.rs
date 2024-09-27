#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Standardized collections across `std` and `no_std` environments.
pub mod collections {
    cfg_if::cfg_if! {
        if #[cfg(feature = "std")] {
            pub use std::collections::{hash_set, HashMap, HashSet};
            use hashbrown as _;
        } else {
            pub use hashbrown::{hash_set, HashMap, HashSet};
        }
    }
}

pub mod config;
pub mod genesis;
pub mod net;
pub mod output;
pub mod receipt;
pub mod safe_head;
pub mod sync;
pub mod transaction;

pub use receipt::{OpTransactionReceipt, OptimismTransactionReceiptFields};
pub use transaction::{OptimismTransactionFields, Transaction};
