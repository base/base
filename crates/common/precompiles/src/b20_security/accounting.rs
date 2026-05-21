//! `SecurityAccounting` — storage port extension for security tokens.

use alloc::string::String;

use alloy_primitives::{B256, U256};
use base_precompile_storage::Result;

use crate::TokenAccounting;

/// Extends [`TokenAccounting`] with security-token-specific storage slots.
///
/// Keys for mappings are pre-hashed to [`B256`] by the caller so that
/// `String` (not a valid [`base_precompile_storage::StorageKey`]) can be used
/// as a logical key while the underlying mapping uses a primitive type.
pub trait SecurityAccounting: TokenAccounting {
    /// Returns the current share-to-tokens ratio scaled to WAD (1e18).
    fn shares_to_tokens_ratio(&self) -> Result<U256>;
    /// Writes a new share-to-tokens ratio.
    fn set_shares_to_tokens_ratio(&mut self, ratio: U256) -> Result<()>;

    /// Returns the security identifier stored under `key` (= `keccak256(identifier_type)`).
    fn security_identifier(&self, key: B256) -> Result<String>;
    /// Writes (or removes when `value` is empty) the security identifier at `key`.
    fn set_security_identifier(&mut self, key: B256, value: String) -> Result<()>;

    /// Returns `true` if `id_hash` (= `keccak256(id)`) has been consumed by `announce`.
    fn is_announcement_id_used(&self, id_hash: B256) -> Result<bool>;
    /// Marks `id_hash` as consumed. Called exactly once per announcement id.
    fn mark_announcement_id_used(&mut self, id_hash: B256) -> Result<()>;
}
