//! SP1 (Succinct) ZK proving backends.
//!
//! Each backend implements [`base_proof_zk_host::ZkProver`] for a different SP1
//! execution target.

use base_proof_zk_host::ZkProverError;
use base_prover_service_protocol::SessionType;
use sp1_sdk::SP1ProofWithPublicValues;

mod provider;
pub use provider::{L1HeadSource, OpSuccinctWitnessProvider, WitnessError, WitnessParams};

mod builder;
pub use builder::{
    SuccinctRpcConfig, SuccinctZkBackendConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, SuccinctZkProversConfig,
};

mod cluster;
pub use cluster::{
    ClusterSessionId, ClusterZkProver, ClusterZkProverConfig, SuccinctClusterBackendConfig,
};

mod network;
pub use network::{NetworkZkProver, NetworkZkProverConfig, SuccinctNetworkBackendConfig};

mod dry_run;
pub use dry_run::{DRY_RUN_SNARK_PREFIX, DRY_RUN_STARK_PREFIX, DryRunZkProver};

/// Encode a downloaded SP1 proof for prover-service clients.
///
/// SNARK/PLONK results must use [`SP1ProofWithPublicValues::bytes`] — the on-chain seal that
/// starts with the SP1 verifier selector. Bincode of the full receipt is not verifiable by
/// `SP1VerifierGateway` and surfaces as `RouteNotFound` on challenge submission.
///
/// Compressed/STARK results keep the full bincode receipt for downstream aggregation.
fn encode_downloaded_proof(
    proof: &SP1ProofWithPublicValues,
    session_type: SessionType,
) -> Result<Vec<u8>, ZkProverError> {
    match session_type {
        SessionType::Snark => Ok(proof.bytes()),
        SessionType::Stark => bincode::serde::encode_to_vec(proof, bincode::config::standard())
            .map_err(|e| {
                ZkProverError::Backend(
                    std::io::Error::other(format!("failed to serialize compressed proof: {e}"))
                        .into(),
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::SessionType;
    use sp1_sdk::{SP1Proof, SP1ProofWithPublicValues, SP1PublicValues};

    use super::encode_downloaded_proof;

    /// Minimal PLONK proof shape: `bytes()` = vkey_hash[:4] || hex_decode(encoded_proof).
    fn plonk_proof(selector: [u8; 4], encoded_proof_hex: &str) -> SP1ProofWithPublicValues {
        let mut plonk_vkey_hash = [0u8; 32];
        plonk_vkey_hash[..4].copy_from_slice(&selector);
        // SP1Proof::Plonk payload is constructed via Default + field overrides so we do not
        // depend on sp1-verifier's PlonkBn254Proof type being re-exported from sp1-sdk.
        let mut proof = SP1ProofWithPublicValues {
            proof: SP1Proof::Plonk(Default::default()),
            public_values: SP1PublicValues::new(),
            sp1_version: "test".to_owned(),
            tee_proof: None,
        };
        match &mut proof.proof {
            SP1Proof::Plonk(plonk) => {
                plonk.encoded_proof = encoded_proof_hex.to_owned();
                plonk.plonk_vkey_hash = plonk_vkey_hash;
            }
            _ => unreachable!(),
        }
        proof
    }

    #[test]
    fn snark_download_uses_onchain_seal_not_bincode() {
        let proof = plonk_proof([0x5a, 0x09, 0x3a, 0x2f], "abcd");
        let snark = encode_downloaded_proof(&proof, SessionType::Snark).expect("snark encode");
        assert_eq!(snark, [0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);

        let stark = encode_downloaded_proof(&proof, SessionType::Stark).expect("stark encode");
        assert_ne!(stark, snark, "compressed path must not return the on-chain seal encoding");
        assert_ne!(&stark[..4], &[0x5a, 0x09, 0x3a, 0x2f]);
    }
}
