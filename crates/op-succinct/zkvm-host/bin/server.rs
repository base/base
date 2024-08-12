use alloy_primitives::B256;
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use client_utils::RawBootInfo;
use host_utils::{fetcher::SP1KonaDataFetcher, get_agg_proof_stdin, get_proof_stdin, ProgramType};
use kona_host::start_server_and_native_client;
use log::info;
use serde::{Deserialize, Serialize};
use sp1_sdk::{
    network::client::NetworkClient,
    proto::network::{ProofMode, ProofStatus as SP1ProofStatus},
    utils, NetworkProver, Prover, SP1Proof, SP1ProofWithPublicValues,
};
use std::{env, fs};
use zkvm_host::utils::fetch_header_preimages;

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../elf/validity-client-elf");
pub const AGG_ELF: &[u8] = include_bytes!("../../elf/aggregation-client-elf");

#[derive(Deserialize, Serialize, Debug)]
struct SpanProofRequest {
    start: u64,
    end: u64,
}

#[derive(Deserialize, Serialize, Debug)]
struct AggProofRequest {
    subproofs: Vec<Vec<u8>>,
    l1_head: B256,
}

#[derive(Serialize, Deserialize, Debug)]
struct ProofResponse {
    proof_id: String,
}

#[derive(Serialize)]
struct ProofStatus {
    status: String,
    proof: Vec<u8>,
}

#[tokio::main]
async fn main() {
    utils::setup_logger();

    let app = Router::new()
        .route("/request_span_proof", post(request_span_proof))
        .route("/request_agg_proof", post(request_agg_proof))
        .route("/status/:proof_id", get(get_proof_status));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    info!("Server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn request_span_proof(
    Json(payload): Json<SpanProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received span proof request: {:?}", payload);
    dotenv::dotenv().ok();
    // ZTODO: Save data fetcher, NetworkProver, and NetworkClient globally
    // and access via Store.
    let data_fetcher = SP1KonaDataFetcher::new();

    let host_cli = data_fetcher
        .get_host_cli_args(payload.start, payload.end, ProgramType::Multi)
        .await?;

    let data_dir = host_cli.data_dir.clone().unwrap();

    // Overwrite existing data directory.
    fs::create_dir_all(&data_dir)?;

    // Start the server and native client.
    start_server_and_native_client(host_cli.clone()).await?;

    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = NetworkProver::new();
    let proof_id = prover
        .request_proof(MULTI_BLOCK_ELF, sp1_stdin, ProofMode::Compressed)
        .await?;

    Ok((StatusCode::OK, Json(ProofResponse { proof_id })))
}

async fn request_agg_proof(
    Json(payload): Json<AggProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received agg proof request: {:?}", payload);
    let mut proofs_with_pv: Vec<SP1ProofWithPublicValues> = payload
        .subproofs
        .iter()
        .map(|sp| bincode::deserialize(sp).unwrap())
        .collect();

    let boot_infos: Vec<RawBootInfo> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.public_values.read::<RawBootInfo>())
        .collect();

    let proofs: Vec<SP1Proof> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.proof.clone())
        .collect();

    let headers = fetch_header_preimages(&boot_infos, payload.l1_head).await?;

    let prover = NetworkProver::new();
    let (_, vkey) = prover.setup(MULTI_BLOCK_ELF);

    let stdin = get_agg_proof_stdin(proofs, boot_infos, headers, &vkey, payload.l1_head).unwrap();

    let proof_id = prover
        .request_proof(AGG_ELF, stdin, ProofMode::Plonk)
        .await?;

    Ok((StatusCode::OK, Json(ProofResponse { proof_id })))
}

async fn get_proof_status(
    Path(proof_id): Path<String>,
) -> Result<(StatusCode, Json<ProofStatus>), AppError> {
    info!("Received proof status request: {:?}", proof_id);
    dotenv::dotenv().ok();
    let private_key = env::var("SP1_PRIVATE_KEY")?;

    let client = NetworkClient::new(&private_key);
    let (status, maybe_proof) = client.get_proof_status(&proof_id).await?;

    let status: SP1ProofStatus = SP1ProofStatus::try_from(status.status)?;
    if status == SP1ProofStatus::ProofFulfilled {
        let proof: SP1ProofWithPublicValues = maybe_proof.unwrap();

        match proof.proof.clone() {
            SP1Proof::Compressed(_) => {
                // If it's a compressed proof, we need to serialize the entire struct with bincode.
                // Note: We're re-serializing the entire struct with bincode here, but this is fine
                // because we're on localhost and the size of the struct is small.
                let proof_bytes = bincode::serialize(&proof).unwrap();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        status: status.as_str_name().to_string(),
                        proof: proof_bytes,
                    }),
                ));
            }
            SP1Proof::Plonk(_) => {
                // If it's a PLONK proof, we need to get the proof bytes that we put on-chain.
                let proof_bytes = proof.bytes();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        status: status.as_str_name().to_string(),
                        proof: proof_bytes,
                    }),
                ));
            }
            _ => (),
        }
    }
    Ok((
        StatusCode::OK,
        Json(ProofStatus {
            status: status.as_str_name().to_string(),
            proof: vec![],
        }),
    ))
}

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", self.0)).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
