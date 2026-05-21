//! `SecurityAccounting` — storage port extension for security tokens.

use alloc::string::String;

use alloy_primitives::{B256, U256};
use base_precompile_storage::Result;

use crate::TokenAccounting;

/// Extends [`TokenAccounting`] with security-token-specific storage slots.
///
/// Security identifiers (ISIN, CUSIP, etc.) are stored and retrieved via
/// [`TokenAccounting::security_identifier`] and
/// [`SecurityAccounting::set_security_identifier_value`].
pub trait SecurityAccounting: TokenAccounting {
    /// Returns the current share-to-tokens ratio scaled to WAD (1e18).
    fn shares_to_tokens_ratio(&self) -> Result<U256>;
    /// Writes a new share-to-tokens ratio.
    fn set_shares_to_tokens_ratio(&mut self, ratio: U256) -> Result<()>;

    /// Writes (or removes when `value` is empty) the security identifier for `identifier_type`.
    fn set_security_identifier_value(&mut self, identifier_type: &str, value: String)
    -> Result<()>;

    /// Returns `true` if `id_hash` (= `keccak256(id)`) has been consumed by `announce`.
    fn is_announcement_id_used(&self, id_hash: B256) -> Result<bool>;
    /// Marks `id_hash` as consumed. Called exactly once per announcement id.
    fn mark_announcement_id_used(&mut self, id_hash: B256) -> Result<()>;
}
