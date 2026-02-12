//! `OnchainVerifier` contract bindings.

use alloy::primitives::{Address, Bytes};
use alloy::providers::RootProvider;
use alloy::sol;
use async_trait::async_trait;

use crate::{ECDSA_SIGNATURE_LENGTH, ECDSA_V_OFFSET, ProposerError};

sol! {
    /// Output proposal structure from the `L2OutputOracle`.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct OutputProposal {
        bytes32 outputRoot;
        uint128 timestamp;
        uint128 l2BlockNumber;
    }

    /// `OnchainVerifier` contract interface.
    #[sol(rpc)]
    interface IOnchainVerifier {
        /// Verifies a TEE proof and returns whether it's valid.
        function verify(
            bytes calldata proofBytes,
            bytes32 rootClaim,
            uint256 l2BlockNumber
        ) external returns (bool valid);

        /// Returns the latest verified output proposal.
        function latestOutputProposal() external view returns (OutputProposal memory);

        /// Returns the output proposal at the given index.
        function getL2Output(uint256 index) external view returns (OutputProposal memory);

        /// Returns the number of output proposals.
        function latestOutputIndex() external view returns (uint256);

        /// Proposes a new output root.
        function proposeL2Output(
            bytes32 outputRoot,
            uint256 l2BlockNumber,
            bytes32 l1BlockHash,
            uint256 l1BlockNumber,
            bytes calldata proof
        ) external;
    }
}

/// Encodes proof bytes with v-value adjustment for ECDSA signatures.
///
/// The enclave returns signatures with v-values of 0 or 1, but Ethereum
/// expects v-values of 27 or 28. This function adjusts the v-value accordingly.
pub fn encode_proof_bytes(mut proof: Vec<u8>) -> Bytes {
    if proof.len() >= ECDSA_SIGNATURE_LENGTH {
        // Adjust v-value from 0/1 to 27/28
        let v_index = ECDSA_SIGNATURE_LENGTH - 1;
        if proof[v_index] < ECDSA_V_OFFSET {
            proof[v_index] += ECDSA_V_OFFSET;
        }
    }
    Bytes::from(proof)
}

/// Decodes proof bytes, converting v-value back from 27/28 to 0/1.
pub fn decode_proof_bytes(proof: &Bytes) -> Vec<u8> {
    let mut bytes = proof.to_vec();
    if bytes.len() >= ECDSA_SIGNATURE_LENGTH {
        let v_index = ECDSA_SIGNATURE_LENGTH - 1;
        if bytes[v_index] >= ECDSA_V_OFFSET {
            bytes[v_index] -= ECDSA_V_OFFSET;
        }
    }
    bytes
}

/// Async trait for reading onchain verifier state.
#[async_trait]
pub trait OnchainVerifierClient: Send + Sync {
    /// Returns the latest output proposal from the contract.
    async fn latest_output_proposal(&self) -> Result<OutputProposal, ProposerError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[allow(missing_debug_implementations)]
pub struct OnchainVerifierContractClient {
    contract: IOnchainVerifier::IOnchainVerifierInstance<RootProvider>,
}

impl OnchainVerifierContractClient {
    /// Creates a new client for the given contract address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Result<Self, ProposerError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IOnchainVerifier::IOnchainVerifierInstance::new(address, provider);
        Ok(Self { contract })
    }
}

#[async_trait]
impl OnchainVerifierClient for OnchainVerifierContractClient {
    async fn latest_output_proposal(&self) -> Result<OutputProposal, ProposerError> {
        self.contract
            .latestOutputProposal()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_proof_bytes_adjusts_v_value() {
        // Create a mock 65-byte signature with v=0
        let mut proof = vec![0u8; 65];
        proof[64] = 0; // v = 0

        let encoded = encode_proof_bytes(proof);
        assert_eq!(encoded[64], 27); // v should be adjusted to 27
    }

    #[test]
    fn test_encode_proof_bytes_preserves_high_v_value() {
        // Create a mock 65-byte signature with v=27 (already adjusted)
        let mut proof = vec![0u8; 65];
        proof[64] = 27;

        let encoded = encode_proof_bytes(proof);
        assert_eq!(encoded[64], 27); // v should remain 27
    }

    #[test]
    fn test_decode_proof_bytes() {
        let mut proof = vec![0u8; 65];
        proof[64] = 28; // v = 28

        let decoded = decode_proof_bytes(&Bytes::from(proof));
        assert_eq!(decoded[64], 1); // v should be adjusted to 1
    }
}
