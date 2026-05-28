use crate::{ProofRequest, TeeProofRequest, ZkProofRequest, proof_request};

impl ProofRequest {
    /// Builds a compressed ZK proof request.
    pub fn compressed(session_id: Option<String>, request: ZkProofRequest) -> Self {
        Self { session_id, request: Some(proof_request::Request::Compressed(request)) }
    }

    /// Builds a SNARK Groth16 ZK proof request.
    pub fn snark_groth16(session_id: Option<String>, request: ZkProofRequest) -> Self {
        Self { session_id, request: Some(proof_request::Request::SnarkGroth16(request)) }
    }

    /// Builds a TEE proof request.
    pub fn tee(session_id: Option<String>, request: TeeProofRequest) -> Self {
        Self { session_id, request: Some(proof_request::Request::Tee(request)) }
    }
}
