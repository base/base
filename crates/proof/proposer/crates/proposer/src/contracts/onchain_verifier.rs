//! `OnchainVerifier` contract bindings.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::RootProvider;
use alloy::sol;
use alloy_sol_types::{SolCall, SolType};
use async_trait::async_trait;

use crate::prover::ProverProposal;
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

/// Builds the ABI-encoded calldata for `IOnchainVerifier.verify(proofBytes, rootClaim, l2BlockNumber)`.
///
/// The `proofBytes` is an inner ABI encoding of `(uint256, bytes32, bytes32, bytes32, bytes)`
/// containing the L1 origin number, L1 origin hash, previous output root, config hash,
/// and the ECDSA signature (with v-value adjusted from 0/1 to 27/28).
pub fn build_verify_calldata(
    l1_origin_number: u64,
    l1_origin_hash: B256,
    prev_output_root: B256,
    config_hash: B256,
    signature: &[u8],
    output_root: B256,
    l2_block_number: U256,
) -> Result<Bytes, ProposerError> {
    if signature.len() < ECDSA_SIGNATURE_LENGTH {
        return Err(ProposerError::Internal(format!(
            "signature too short: expected at least {ECDSA_SIGNATURE_LENGTH} bytes, got {}",
            signature.len()
        )));
    }

    // Adjust v-value from 0/1 to 27/28
    let proof_bytes = encode_proof_bytes(signature.to_vec());

    // Inner encoding: (uint256, bytes32, bytes32, bytes32, bytes)
    type InnerTuple = (
        alloy_sol_types::sol_data::Uint<256>,
        alloy_sol_types::sol_data::FixedBytes<32>,
        alloy_sol_types::sol_data::FixedBytes<32>,
        alloy_sol_types::sol_data::FixedBytes<32>,
        alloy_sol_types::sol_data::Bytes,
    );
    let inner = InnerTuple::abi_encode_params(&(
        U256::from(l1_origin_number),
        l1_origin_hash.0,
        prev_output_root.0,
        config_hash.0,
        proof_bytes.to_vec(),
    ));

    // Outer encoding: verify(proofBytes, rootClaim, l2BlockNumber)
    let call = IOnchainVerifier::verifyCall {
        proofBytes: Bytes::from(inner),
        rootClaim: output_root,
        l2BlockNumber: l2_block_number,
    };

    Ok(Bytes::from(call.abi_encode()))
}

/// Convenience wrapper that builds verify calldata from a [`ProverProposal`].
pub fn build_verify_calldata_from_proposal(
    proposal: &ProverProposal,
) -> Result<Bytes, ProposerError> {
    build_verify_calldata(
        proposal.to.l1origin.number,
        proposal.to.l1origin.hash,
        proposal.output.prev_output_root,
        proposal.output.config_hash,
        &proposal.output.signature,
        proposal.output.output_root,
        proposal.output.l2_block_number,
    )
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

    #[test]
    fn test_build_verify_calldata_selector() {
        let signature = vec![0u8; 65];
        let calldata = build_verify_calldata(
            100,
            B256::repeat_byte(0x01),
            B256::repeat_byte(0x02),
            B256::repeat_byte(0x03),
            &signature,
            B256::repeat_byte(0x04),
            U256::from(200),
        )
        .unwrap();

        // First 4 bytes should be the function selector for verify()
        let expected_selector = IOnchainVerifier::verifyCall::SELECTOR;
        assert_eq!(&calldata[..4], &expected_selector);
    }

    #[test]
    fn test_build_verify_calldata_v_value_adjustment() {
        let mut signature = vec![0u8; 65];
        signature[64] = 0; // v = 0, should be adjusted to 27

        let calldata = build_verify_calldata(
            100,
            B256::ZERO,
            B256::ZERO,
            B256::ZERO,
            &signature,
            B256::ZERO,
            U256::from(200),
        )
        .unwrap();

        // The calldata should be non-empty and start with the right selector
        assert!(!calldata.is_empty());
        assert_eq!(&calldata[..4], &IOnchainVerifier::verifyCall::SELECTOR);
    }

    #[test]
    fn test_build_verify_calldata_short_signature() {
        let signature = vec![0u8; 32]; // Too short
        let result = build_verify_calldata(
            100,
            B256::ZERO,
            B256::ZERO,
            B256::ZERO,
            &signature,
            B256::ZERO,
            U256::from(200),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_build_verify_calldata_empty_signature() {
        let result = build_verify_calldata(
            100,
            B256::ZERO,
            B256::ZERO,
            B256::ZERO,
            &[],
            B256::ZERO,
            U256::from(200),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_build_verify_calldata_from_proposal() {
        use crate::prover::types::test_helpers::test_proposal;

        let proposal = test_proposal(101, 200, false);
        let calldata = build_verify_calldata_from_proposal(&proposal).unwrap();

        assert_eq!(&calldata[..4], &IOnchainVerifier::verifyCall::SELECTOR);
    }
}
