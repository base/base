//! Request types for composed ZK proof flows.

use alloy_primitives::Address;
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProveBlockRangeRequest, SnarkGroth16ProofRequest,
    ZkProofRequest,
};

const RANGE_SESSION_LABEL: &str = "range";

const AGGREGATION_SESSION_LABEL: &str = "aggregation";

/// Logical request for a Groth16 proof over an L2 block range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Groth16RangeProofRequest {
    /// Stable parent session identifier for the logical Groth16 proof flow.
    pub session_id: String,
    /// ZK range proof parameters.
    pub proof: ZkProofRequest,
    /// On-chain prover address to embed in the Groth16 aggregation proof.
    pub prover_address: Address,
}

impl Groth16RangeProofRequest {
    /// Create a logical Groth16 range proof request.
    pub fn new(
        session_id: impl Into<String>,
        proof: ZkProofRequest,
        prover_address: Address,
    ) -> Self {
        Self { session_id: session_id.into(), proof, prover_address }
    }

    /// Return the child session id used for the range proof stage.
    pub fn range_session_id(&self) -> String {
        format!("{}:{RANGE_SESSION_LABEL}", self.session_id)
    }

    /// Return the child session id used for the Groth16 aggregation proof stage.
    pub fn aggregation_session_id(&self) -> String {
        format!("{}:{AGGREGATION_SESSION_LABEL}", self.session_id)
    }

    /// Build the prover-service request for the compressed range proof stage.
    pub fn range_prove_block_request(&self) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: self.range_session_id(),
                request: ProofRequestKind::Compressed(self.proof.clone()),
            },
        }
    }

    /// Build the prover-service request for the Groth16 aggregation proof stage.
    pub fn aggregation_prove_block_request(&self) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: self.aggregation_session_id(),
                request: ProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
                    proof: self.proof.clone(),
                    prover_address: self.prover_address,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::ZkVm;

    use super::*;

    fn proof_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 10,
            number_of_blocks_to_prove: 2,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: Some(2),
            zk_vm: ZkVm::Sp1,
        }
    }

    #[test]
    fn builds_range_and_aggregation_requests() {
        let prover_address = Address::repeat_byte(0x11);
        let request =
            Groth16RangeProofRequest::new("parent-session", proof_request(), prover_address);

        let range = request.range_prove_block_request();
        assert_eq!(range.proof.session_id, "parent-session:range");
        assert!(matches!(range.proof.request, ProofRequestKind::Compressed(_)));

        let aggregation = request.aggregation_prove_block_request();
        assert_eq!(aggregation.proof.session_id, "parent-session:aggregation");
        let ProofRequestKind::SnarkGroth16(aggregation) = aggregation.proof.request else {
            panic!("expected Groth16 aggregation request");
        };
        assert_eq!(aggregation.prover_address, prover_address);
    }
}
