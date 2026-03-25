//! A disabled (no-op) safe head database.

use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{SafeDBError, SafeDBReader, SafeHeadListener, SafeHeadResponse};

/// A disabled safe head database that does nothing.
///
/// Used when safe head tracking is not enabled on a node.
///
/// Write operations ([`SafeHeadListener`]) silently succeed (no-op) so that the
/// derivation pipeline does not need to special-case the disabled state.
/// Read operations ([`SafeDBReader`]) return [`SafeDBError::Disabled`] so that
/// the RPC layer can propagate a clear "feature disabled" error to callers.
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledSafeDB;

impl SafeHeadListener for DisabledSafeDB {
    /// No-op. Always succeeds without recording anything.
    fn safe_head_updated(
        &self,
        _safe_head: L2BlockInfo,
        _l1_block: BlockInfo,
    ) -> Result<(), SafeDBError> {
        Ok(())
    }

    /// No-op. Always succeeds without recording anything.
    fn safe_head_reset(&self, _reset_safe_head: L2BlockInfo) -> Result<(), SafeDBError> {
        Ok(())
    }
}

impl SafeDBReader for DisabledSafeDB {
    /// Always returns [`SafeDBError::Disabled`].
    fn safe_head_at_l1(&self, _l1_block_num: u64) -> Result<SafeHeadResponse, SafeDBError> {
        Err(SafeDBError::Disabled)
    }
}
