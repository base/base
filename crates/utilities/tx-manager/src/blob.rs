//! EIP-4844 blob transaction sidecar construction.
//!
//! [`BlobTxBuilder`] wraps alloy's KZG sidecar API to produce
//! [`BlobTransactionSidecar`] (legacy, 1 proof/blob) or
//! [`BlobTransactionSidecarVariant::Eip7594`] (cell proofs, 128 proofs/blob)
//! depending on a configurable activation timestamp.

use std::sync::Arc;

use alloy_eips::{
    eip4844::{Blob, BlobTransactionSidecar, env_settings::EnvKzgSettings},
    eip7594::{
        BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant, MAX_BLOBS_PER_TX_FUSAKA,
    },
};

use crate::TxManagerError;

/// Maximum number of blobs allowed per transaction.
///
/// Set to [`MAX_BLOBS_PER_TX_FUSAKA`] (6), the Fusaka per-transaction limit.
pub const MAX_BLOBS_PER_TX: usize = MAX_BLOBS_PER_TX_FUSAKA as usize;

/// Builder for EIP-4844 blob-carrying transaction sidecars.
///
/// Wraps alloy's KZG primitives to construct either legacy (1 proof/blob)
/// or EIP-7594 cell-proof (128 proofs/blob) sidecars based on a
/// configurable activation timestamp.
#[derive(Debug, Clone)]
pub struct BlobTxBuilder {
    /// Unix timestamp at or after which cell proofs are used.
    /// `u64::MAX` disables cell proofs (always legacy).
    cell_proofs_activation_timestamp: u64,
}

impl BlobTxBuilder {
    /// Creates a new [`BlobTxBuilder`].
    ///
    /// Pass `u64::MAX` to disable cell proofs (always produce legacy sidecars).
    #[must_use]
    pub const fn new(cell_proofs_activation_timestamp: u64) -> Self {
        Self { cell_proofs_activation_timestamp }
    }

    /// Returns `true` when EIP-7594 cell proofs should be used.
    ///
    /// Cell proofs are active when `block_timestamp` is at or past the
    /// configured activation timestamp.
    #[must_use]
    pub const fn should_use_cell_proofs(&self, block_timestamp: u64) -> bool {
        if self.cell_proofs_activation_timestamp == u64::MAX {
            return false;
        }
        block_timestamp >= self.cell_proofs_activation_timestamp
    }

    /// Returns `true` when `sidecar`'s proof variant matches what
    /// [`should_use_cell_proofs`](Self::should_use_cell_proofs) would select
    /// for the given `block_timestamp`.
    ///
    /// Use this to detect stale cached sidecars across fork boundaries
    /// (e.g. the `cell_proofs_activation_timestamp` was crossed during an
    /// active send loop).
    #[must_use]
    pub const fn is_sidecar_valid(
        &self,
        sidecar: &BlobTransactionSidecarVariant,
        block_timestamp: u64,
    ) -> bool {
        self.should_use_cell_proofs(block_timestamp) == sidecar.is_eip7594()
    }

    /// Builds a [`BlobTransactionSidecarVariant`], automatically selecting
    /// legacy or EIP-7594 cell proofs based on [`should_use_cell_proofs`](Self::should_use_cell_proofs).
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Unsupported`] if KZG computation fails.
    pub fn make_sidecar_auto(
        &self,
        blobs: Arc<Vec<Blob>>,
        block_timestamp: u64,
    ) -> Result<BlobTransactionSidecarVariant, TxManagerError> {
        let settings = EnvKzgSettings::Default;
        let kzg = settings.get();
        let blobs = Arc::unwrap_or_clone(blobs);

        if self.should_use_cell_proofs(block_timestamp) {
            let eip7594 = BlobTransactionSidecarEip7594::try_from_blobs_with_settings(blobs, kzg)
                .map_err(|e| {
                TxManagerError::Unsupported(format!("EIP-7594 cell proof computation failed: {e}"))
            })?;
            Ok(BlobTransactionSidecarVariant::Eip7594(eip7594))
        } else {
            let sidecar = BlobTransactionSidecar::try_from_blobs_with_settings(blobs, kzg)
                .map_err(|e| {
                    TxManagerError::Unsupported(format!("KZG sidecar construction failed: {e}"))
                })?;
            Ok(BlobTransactionSidecarVariant::Eip4844(sidecar))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::{eip4844::Blob, eip7594::CELLS_PER_EXT_BLOB};

    use super::*;

    /// Helper: creates a builder with cell proofs disabled.
    fn legacy_builder() -> BlobTxBuilder {
        BlobTxBuilder::new(u64::MAX)
    }

    /// Helper: creates a builder with cell proofs always active.
    fn cell_proofs_builder() -> BlobTxBuilder {
        BlobTxBuilder::new(0)
    }

    #[test]
    fn make_sidecar_auto_single_blob_legacy() {
        let builder = legacy_builder();
        let variant = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 0).unwrap();
        let sidecar = variant.as_eip4844().expect("expected Eip4844 variant");
        assert_eq!(sidecar.blobs.len(), 1);
        assert_eq!(sidecar.commitments.len(), 1);
        assert_eq!(sidecar.proofs.len(), 1);
    }

    #[test]
    fn make_sidecar_auto_two_blobs_legacy() {
        let builder = legacy_builder();
        let variant =
            builder.make_sidecar_auto(Arc::new(vec![Blob::default(), Blob::default()]), 0).unwrap();
        let sidecar = variant.as_eip4844().expect("expected Eip4844 variant");
        assert_eq!(sidecar.blobs.len(), 2);
        assert_eq!(sidecar.commitments.len(), 2);
        assert_eq!(sidecar.proofs.len(), 2);
    }

    #[test]
    fn make_sidecar_auto_six_blobs_legacy() {
        let builder = legacy_builder();
        let variant = builder.make_sidecar_auto(Arc::new(vec![Blob::default(); 6]), 0).unwrap();
        let sidecar = variant.as_eip4844().expect("expected Eip4844 variant");
        assert_eq!(sidecar.blobs.len(), 6);
        assert_eq!(sidecar.commitments.len(), 6);
        assert_eq!(sidecar.proofs.len(), 6);
    }

    #[test]
    fn versioned_hashes_use_0x01_version_byte() {
        let builder = legacy_builder();
        let variant =
            builder.make_sidecar_auto(Arc::new(vec![Blob::default(), Blob::default()]), 0).unwrap();
        for hash in variant.versioned_hashes() {
            assert_eq!(hash.0[0], 0x01, "versioned hash should start with 0x01, got: {hash}");
        }
    }

    #[test]
    fn should_use_cell_proofs_disabled() {
        let builder = BlobTxBuilder::new(u64::MAX);
        assert!(!builder.should_use_cell_proofs(1_000_000));
    }

    #[test]
    fn should_use_cell_proofs_past() {
        let builder = BlobTxBuilder::new(1_000);
        assert!(builder.should_use_cell_proofs(1_001));
    }

    #[test]
    fn should_use_cell_proofs_exactly_at_activation() {
        let builder = BlobTxBuilder::new(1_000);
        assert!(builder.should_use_cell_proofs(1_000));
    }

    #[test]
    fn should_use_cell_proofs_one_second_before() {
        let builder = BlobTxBuilder::new(1_000);
        assert!(!builder.should_use_cell_proofs(999));
    }

    #[test]
    fn should_use_cell_proofs_future() {
        let builder = BlobTxBuilder::new(u64::MAX - 1);
        assert!(!builder.should_use_cell_proofs(1_000_000));
    }

    #[test]
    fn make_sidecar_auto_legacy() {
        let builder = legacy_builder();
        let variant = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 0).unwrap();
        assert!(variant.is_eip4844(), "expected Eip4844 variant");
        let sidecar = variant.as_eip4844().unwrap();
        assert_eq!(sidecar.proofs.len(), 1);
    }

    #[test]
    fn make_sidecar_auto_cell_proofs() {
        let builder = cell_proofs_builder();
        let variant = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 1_000).unwrap();
        assert!(variant.is_eip7594(), "expected Eip7594 variant");
        let sidecar = variant.as_eip7594().unwrap();
        // 128 cell proofs per blob.
        assert_eq!(sidecar.cell_proofs.len(), CELLS_PER_EXT_BLOB);
    }

    #[test]
    fn blob_tx_builder_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BlobTxBuilder>();
    }

    // ── is_sidecar_valid ────────────────────────────────────────────────

    #[test]
    fn is_sidecar_valid_matches_legacy_before_activation() {
        let builder = BlobTxBuilder::new(1_000);
        let sidecar = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 999).unwrap();
        assert!(builder.is_sidecar_valid(&sidecar, 999));
    }

    #[test]
    fn is_sidecar_valid_matches_cell_proofs_after_activation() {
        let builder = BlobTxBuilder::new(1_000);
        let sidecar = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 1_000).unwrap();
        assert!(builder.is_sidecar_valid(&sidecar, 1_000));
    }

    #[test]
    fn is_sidecar_valid_rejects_legacy_after_activation() {
        let builder = BlobTxBuilder::new(1_000);
        // Build a legacy sidecar (pre-activation).
        let sidecar = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 999).unwrap();
        assert!(sidecar.is_eip4844());
        // Validate against a post-activation timestamp — should be invalid.
        assert!(!builder.is_sidecar_valid(&sidecar, 1_000));
    }

    #[test]
    fn is_sidecar_valid_rejects_cell_proofs_before_activation() {
        let builder = BlobTxBuilder::new(1_000);
        // Build a cell-proof sidecar (post-activation).
        let sidecar = builder.make_sidecar_auto(Arc::new(vec![Blob::default()]), 1_000).unwrap();
        assert!(sidecar.is_eip7594());
        // Validate against a pre-activation timestamp — should be invalid.
        assert!(!builder.is_sidecar_valid(&sidecar, 999));
    }

    #[test]
    fn sidecar_cache_invalidated_across_fork_transition() {
        let builder = BlobTxBuilder::new(1_000);
        let blobs = Arc::new(vec![Blob::default()]);

        // Build pre-fork sidecar.
        let pre_fork = builder.make_sidecar_auto(Arc::clone(&blobs), 999).unwrap();
        assert!(pre_fork.is_eip4844());
        assert!(builder.is_sidecar_valid(&pre_fork, 999));

        // After fork activation the cached sidecar is stale.
        assert!(!builder.is_sidecar_valid(&pre_fork, 1_000));

        // Rebuild with post-fork timestamp produces a valid sidecar.
        let post_fork = builder.make_sidecar_auto(blobs, 1_000).unwrap();
        assert!(post_fork.is_eip7594());
        assert!(builder.is_sidecar_valid(&post_fork, 1_000));
    }
}
