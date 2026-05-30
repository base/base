//! Shared helpers for prover-service integration tests.

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, bail};
use base_proof_primitives::ProofRequest as PrimitiveProofRequest;
pub(crate) use base_prover_service::ProveBlockRequest;
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProveBlockRangeRequest, ProveBlockRangeResponse,
    ProverRequesterApiClient, SnarkGroth16ProofRequest, TeeKind, TeeProofRequest, ZkProofRequest,
    ZkVm,
};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};

pub(crate) fn connect() -> HttpClient {
    let addr = std::env::var("PROVER_RPC_ADDR")
        .or_else(|_| std::env::var("PROVER_GRPC_ADDR"))
        .unwrap_or_else(|_| "http://localhost:9000".to_string());

    HttpClientBuilder::default()
        .build(addr)
        .expect("failed to connect to prover-service - is it running?")
}

pub(crate) async fn prove_block(
    client: &HttpClient,
    request: ProveBlockRequest,
) -> Result<ProveBlockRangeResponse> {
    let request = to_prove_block_range_request(request)?;
    client.prove_block_range(request).await.context("prove_block_range failed")
}

fn to_prove_block_range_request(request: ProveBlockRequest) -> Result<ProveBlockRangeRequest> {
    let l1_head = request
        .l1_head
        .as_deref()
        .map(str::parse::<B256>)
        .transpose()
        .context("l1_head must be a 0x-prefixed 32-byte hash")?;
    let proof = ZkProofRequest {
        start_block_number: request.start_block_number,
        number_of_blocks_to_prove: request.number_of_blocks_to_prove,
        sequence_window: request.sequence_window,
        l1_head,
        intermediate_root_interval: request.intermediate_root_interval,
        zk_vm: ZkVm::Sp1,
    };
    let body = match request.proof_type {
        3 => ProofRequestKind::Compressed(proof),
        4 => {
            let prover_address = request
                .prover_address
                .as_deref()
                .context("prover_address is required for SNARK_GROTH16 proofs")?
                .parse::<Address>()
                .context("prover_address must be a valid Ethereum address")?;
            ProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest { proof, prover_address })
        }
        -1 => ProofRequestKind::Tee(TeeProofRequest {
            proof: PrimitiveProofRequest {
                l1_head: B256::ZERO,
                agreed_l2_head_hash: B256::ZERO,
                agreed_l2_output_root: B256::ZERO,
                claimed_l2_output_root: B256::ZERO,
                claimed_l2_block_number: 0,
                proposer: Address::ZERO,
                intermediate_block_interval: 0,
                l1_head_number: 0,
                image_hash: B256::ZERO,
            },
            tee_kind: TeeKind::AwsNitro,
        }),
        proof_type => bail!("invalid proof_type {proof_type}"),
    };

    Ok(ProveBlockRangeRequest {
        proof: ProofRequest { session_id: request.session_id, request: body },
    })
}
