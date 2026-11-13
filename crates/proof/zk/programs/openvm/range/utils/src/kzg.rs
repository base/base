//! KZG verification on the OpenVM BLS12-381 pairing extension.

use base_proof_zk_utils::{KzgVerifier, witness::BlobData};
use openvm_kzg::{Blob, Bytes32, Bytes48, KzgError, KzgProof, KzgSettings, get_kzg_settings};
use revm_precompile::{Crypto, PrecompileHalt};

/// [`KzgVerifier`] backed by `openvm-kzg`, so blob and point-evaluation checks run as
/// OpenVM intrinsics.
#[derive(Debug)]
pub struct OpenVmKzg {
    kzg_settings: KzgSettings,
}

impl Default for OpenVmKzg {
    fn default() -> Self {
        Self { kzg_settings: get_kzg_settings() }
    }
}

impl KzgVerifier for OpenVmKzg {
    type Error = KzgError;

    fn verify_blob_kzg_proof_batch(&self, blob_data: BlobData) -> Result<bool, Self::Error> {
        let blobs =
            blob_data.blobs.iter().map(|b| Blob::from_slice(&b.0)).collect::<Result<_, _>>()?;
        let commitments = blob_data
            .commitments
            .iter()
            .map(|c| Bytes48::from_slice(&c.0))
            .collect::<Result<_, _>>()?;
        let proofs =
            blob_data.proofs.iter().map(|p| Bytes48::from_slice(&p.0)).collect::<Result<_, _>>()?;
        KzgProof::verify_blob_kzg_proof_batch(blobs, commitments, proofs, &self.kzg_settings)
    }
}

impl Crypto for OpenVmKzg {
    fn verify_kzg_proof(
        &self,
        z: &[u8; 32],
        y: &[u8; 32],
        commitment: &[u8; 48],
        proof: &[u8; 48],
    ) -> Result<(), PrecompileHalt> {
        let z = Bytes32::from_slice(z).map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;
        let y = Bytes32::from_slice(y).map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;
        let commitment = Bytes48::from_slice(commitment)
            .map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;
        let proof =
            Bytes48::from_slice(proof).map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;

        let valid = KzgProof::verify_kzg_proof(&commitment, &z, &y, &proof, &self.kzg_settings)
            .map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;

        if !valid {
            return Err(PrecompileHalt::BlobVerifyKzgProofFailed);
        }

        Ok(())
    }
}
