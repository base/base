//! Traits, validation methods, and helper types used to abstract over engine types.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// Re-export [`ExecutionPayload`] moved to `base_execution_payload_types`
#[cfg(feature = "std")]
pub use base_execution_evm_blocks::ConvertTx;
pub use base_execution_evm_blocks::{ExecutableTxIterator, ExecutableTxTuple};
pub use base_execution_payload_types::ExecutionPayload;

mod error;
pub use error::*;

mod forkchoice;
pub use forkchoice::{ForkchoiceStateHash, ForkchoiceStateTracker, ForkchoiceStatus};

#[cfg(feature = "std")]
mod message;
#[cfg(feature = "std")]
pub use message::*;

mod event;
pub use event::*;

mod invalid_block_hook;
pub use invalid_block_hook::{InvalidBlockHook, InvalidBlockHooks, NoopInvalidBlockHook};

pub mod config;
pub use config::*;
