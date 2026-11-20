//! EIP-7594 blob transaction sidecar construction.
//!
//! [`BlobTxBuilder`] wraps alloy's KZG sidecar API to produce
//! [`BlobTransactionSidecarEip7594`] sidecars
//! (cell proofs, 128 proofs/blob) on all supported networks.

use alloy_eips::{
    eip4844::Blob,
    eip7594::{BlobTransactionSidecarEip7594, MAX_BLOBS_PER_TX_FUSAKA},
};

use crate::TxManagerError;

/// Maximum number of blobs allowed per transaction.
///
/// Set to [`MAX_BLOBS_PER_TX_FUSAKA`] (6), the Fusaka per-transaction limit.
pub const MAX_BLOBS_PER_TX: usize = MAX_BLOBS_PER_TX_FUSAKA as usize;

/// Builder for EIP-7594 blob sidecars.
#[derive(Debug)]
pub struct BlobTxBuilder;

impl BlobTxBuilder {
    /// Builds an EIP-7594 cell-proof sidecar from boxed blobs.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Unsupported`] if KZG computation fails.
    pub fn build_sidecar(
        blobs: &[Box<Blob>],
    ) -> Result<BlobTransactionSidecarEip7594, TxManagerError> {
        #[cfg(feature = "kzg")]
        {
            let unboxed: Vec<Blob> = blobs.iter().map(|b| *b.as_ref()).collect();
            BlobTransactionSidecarEip7594::try_from_blobs(unboxed).map_err(|e| {
                TxManagerError::Unsupported(format!("EIP-7594 cell proof computation failed: {e}"))
            })
        }
        #[cfg(not(feature = "kzg"))]
        {
            let _ = blobs;
            Err(TxManagerError::Unsupported(
                "blob sidecar computation requires the `kzg` feature on base-tx-manager"
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip4844::Blob;
    #[cfg(feature = "kzg")]
    use alloy_eips::eip7594::CELLS_PER_EXT_BLOB;
    #[cfg(feature = "kzg")]
    use rstest::rstest;

    use super::*;

    #[cfg(not(feature = "kzg"))]
    #[test]
    fn build_sidecar_returns_unsupported_without_kzg_feature() {
        let err = BlobTxBuilder::build_sidecar(&[Box::<Blob>::default()]).unwrap_err();
        assert!(matches!(err, TxManagerError::Unsupported(_)));
    }

    #[cfg(feature = "kzg")]
    #[rstest]
    #[case::single_blob(1)]
    #[case::two_blobs(2)]
    #[case::six_blobs(6)]
    fn build_sidecar_n_blobs_uses_cell_proofs(#[case] n: usize) {
        let blobs: Vec<Box<Blob>> = (0..n).map(|_| Box::default()).collect();
        let sidecar = BlobTxBuilder::build_sidecar(&blobs).unwrap();
        assert_eq!(sidecar.cell_proofs.len(), n * CELLS_PER_EXT_BLOB);
    }
}
