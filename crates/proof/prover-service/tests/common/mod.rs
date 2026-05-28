//! Shared helpers for prover-service integration tests.

pub(crate) use base_prover_service::ProveBlockRequest;
use base_prover_service::{
    ProofRequest, SnarkGroth16ProofRequest, SubmitProofRequest, SubmitProofResponse,
    TeeProofRequest, ZkProofRequest, ZkVm, proof_request,
    prover_service_client::ProverServiceClient,
};
use tonic::{Response, Status, transport::Channel};

#[async_trait::async_trait]
pub(crate) trait ProverServiceClientCompat {
    async fn prove_block(
        &mut self,
        request: ProveBlockRequest,
    ) -> Result<Response<SubmitProofResponse>, Status>;
}

#[async_trait::async_trait]
impl ProverServiceClientCompat for ProverServiceClient<Channel> {
    async fn prove_block(
        &mut self,
        request: ProveBlockRequest,
    ) -> Result<Response<SubmitProofResponse>, Status> {
        self.submit_proof(to_submit_proof_request(request)).await
    }
}

fn to_submit_proof_request(request: ProveBlockRequest) -> SubmitProofRequest {
    let zk_vm =
        if matches!(request.proof_type, 3 | 4) { ZkVm::Sp1.into() } else { request.proof_type };
    let proof = ZkProofRequest {
        start_block_number: request.start_block_number,
        number_of_blocks_to_prove: request.number_of_blocks_to_prove,
        sequence_window: request.sequence_window,
        l1_head: request.l1_head,
        intermediate_root_interval: request.intermediate_root_interval,
        zk_vm,
    };
    let body = match request.proof_type {
        4 => proof_request::Request::SnarkGroth16(SnarkGroth16ProofRequest {
            proof: Some(proof),
            prover_address: request.prover_address.unwrap_or_default(),
        }),
        -1 => proof_request::Request::Tee(TeeProofRequest::default()),
        _ => proof_request::Request::Compressed(proof),
    };

    SubmitProofRequest {
        proof: Some(ProofRequest { session_id: request.session_id, request: Some(body) }),
    }
}
