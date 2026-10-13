//! Development-only native BLS transaction type (EIP-2718 type `0x7a`).
//!
//! Provides the unsigned [`TxBls`] (signed as `Signed<TxBls>` with a normal outer ECDSA
//! signature), the [`BlsPayload`] input codec with its [`BlsTransfer`] records, and the shared
//! [`BlsConstants`]. Only the development chain may execute or admit this type; parsing is
//! chain-agnostic.

mod constants;
pub use constants::BlsConstants;

mod payload;
pub use payload::{BlsPayload, BlsPayloadError, BlsTransfer};

mod tx;
pub use tx::TxBls;
