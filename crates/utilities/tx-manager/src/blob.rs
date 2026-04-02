//! EIP-4844 blob transaction sidecar construction.
//!
//! [`BlobTxBuilder`] wraps alloy's KZG sidecar API to produce
//! [`BlobTransactionSidecarEip7594`] sidecars
//! (cell proofs, 128 proofs/blob) on all supported networks.

use std::sync::Arc;

use alloy_eips::{
    eip4844::{Blob, env_settings::EnvKzgSettings},
    eip7594::{BlobTransactionSidecarEip7594, MAX_BLOBS_PER_TX_FUSAKA},
};

use crate::TxManagerError;

/// Maximum number of blobs allowed per transaction.
///
/// Set to [`MAX_BLOBS_PER_TX_FUSAKA`] (6), the Fusaka per-transaction limit.
pub const MAX_BLOBS_PER_TX: usize = MAX_BLOBS_PER_TX_FUSAKA as usize;

/// Builder for Osaka-era EIP-4844 blob sidecars.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlobTxBuilder;

impl BlobTxBuilder {
    /// Creates a new [`BlobTxBuilder`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Builds an Osaka-era EIP-7594 cell-proof sidecar.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Unsupported`] if KZG computation fails.
    pub fn build_sidecar(
        &self,
        blobs: Arc<Vec<Box<Blob>>>,
    ) -> Result<BlobTransactionSidecarEip7594, TxManagerError> {
        let settings = EnvKzgSettings::Default;
        let kzg = settings.get();
        let unboxed: Vec<Blob> = Arc::unwrap_or_clone(blobs).into_iter().map(|b| *b).collect();

        BlobTransactionSidecarEip7594::try_from_blobs_with_settings(unboxed, kzg).map_err(|e| {
            TxManagerError::Unsupported(format!("EIP-7594 cell proof computation failed: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::{eip4844::Blob, eip7594::CELLS_PER_EXT_BLOB};
    use rstest::rstest;

    use super::*;

    fn builder() -> BlobTxBuilder {
        BlobTxBuilder::new()
    }

    #[rstest]
    #[case::single_blob(1)]
    #[case::two_blobs(2)]
    #[case::six_blobs(6)]
    fn build_sidecar_n_blobs_uses_cell_proofs(#[case] n: usize) {
        let builder = builder();
        let blobs: Vec<Box<Blob>> = (0..n).map(|_| Box::default()).collect();
        let sidecar = builder.build_sidecar(Arc::new(blobs)).unwrap();
        assert_eq!(sidecar.cell_proofs.len(), n * CELLS_PER_EXT_BLOB);
    }

    #[test]
    fn blob_tx_builder_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BlobTxBuilder>();
    }
}
