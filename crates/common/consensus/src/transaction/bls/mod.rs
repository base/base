//! Native BLS transaction types.
//!
//! Provides the canonical transfer [`TxBlsTransfer`] (EIP-2718 type `0x7b`, sealed rather than
//! ECDSA-signed) and its pooled wire form [`BlsTransferWithSignature`], the development-only
//! [`TxBls`] (type `0x7a`, signed as `Signed<TxBls>` with a normal outer ECDSA signature), the
//! [`BlsPayload`] input codec with its [`BlsTransfer`] records, and the shared [`BlsConstants`].
//! Parsing is chain-agnostic; execution and admission own chain gating and BLS verification.

mod constants;
pub use constants::BlsConstants;

mod payload;
pub use payload::{BlsPayload, BlsPayloadError, BlsTransfer};

mod tx;
pub use tx::TxBls;

mod transfer;
pub use transfer::TxBlsTransfer;

mod with_signature;
pub use with_signature::BlsTransferWithSignature;
