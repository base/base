use alloy_primitives::hex;
use axum::{
    extract::{DefaultBodyLimit, Path},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose, Engine as _};
use client_utils::{RawBootInfo, BOOT_INFO_SIZE};
use host_utils::{
    fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin, get_proof_stdin, ProgramType,
};
use log::info;
use op_succinct_proposer::run_native_host;
use serde::{Deserialize, Deserializer, Serialize};
use sp1_sdk::{
    network::client::NetworkClient,
    proto::network::{ProofMode, ProofStatus as SP1ProofStatus},
    utils, NetworkProver, Prover, SP1Proof, SP1ProofWithPublicValues,
};
use std::{env, fs, time::Duration};
use tower_http::limit::RequestBodyLimitLayer;

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/range-elf");
pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");

#[derive(Deserialize, Serialize, Debug)]
struct SpanProofRequest {
    start: u64,
    end: u64,
}

#[derive(Deserialize, Serialize, Debug)]
struct AggProofRequest {
    #[serde(deserialize_with = "deserialize_base64_vec")]
    subproofs: Vec<Vec<u8>>,
    head: String,
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
        .route("/status/:proof_id", get(get_proof_status))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(102400 * 1024 * 1024));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    info!("Server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

/// Request a proof for a span of blocks.
async fn request_span_proof(
    Json(payload): Json<SpanProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received span proof request: {:?}", payload);
    dotenv::dotenv().ok();
    // TODO: Save data fetcher, NetworkProver, and NetworkClient globally
    // and access via Store.
    let data_fetcher = OPSuccinctDataFetcher::new();

    let host_cli =
        data_fetcher.get_host_cli_args(payload.start, payload.end, ProgramType::Multi).await?;

    let data_dir = host_cli.data_dir.clone().unwrap();

    // Overwrite existing data directory.
    fs::create_dir_all(&data_dir)?;

    // Start the server and native client with a timeout.
    // Note: Ideally, the server should call out to a separate process that executes the native
    // host, and return an ID that the client can poll on to check if the proof was submitted.
    // TODO: If this fails, we should definitely NOT request a proof! Otherwise, we get execution
    // failures on the cluster.
    run_native_host(&host_cli, Duration::from_secs(60)).await?;

    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = NetworkProver::new();
    let proof_id = prover.request_proof(MULTI_BLOCK_ELF, sp1_stdin, ProofMode::Compressed).await?;

    Ok((StatusCode::OK, Json(ProofResponse { proof_id })))
}

/// Request an aggregation proof for a set of subproofs.
async fn request_agg_proof(
    Json(payload): Json<AggProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received agg proof request");
    let mut proofs_with_pv: Vec<SP1ProofWithPublicValues> =
        payload.subproofs.iter().map(|sp| bincode::deserialize(sp).unwrap()).collect();

    let boot_infos: Vec<RawBootInfo> = proofs_with_pv
        .iter_mut()
        .map(|proof| {
            let mut boot_info_buf = [0u8; BOOT_INFO_SIZE];
            proof.public_values.read_slice(&mut boot_info_buf);
            RawBootInfo::abi_decode(&boot_info_buf).unwrap()
        })
        .collect();

    let proofs: Vec<SP1Proof> =
        proofs_with_pv.iter_mut().map(|proof| proof.proof.clone()).collect();

    let l1_head_bytes =
        hex::decode(payload.head.strip_prefix("0x").expect("Invalid L1 head, no 0x prefix."))?;
    let l1_head: [u8; 32] = l1_head_bytes.try_into().unwrap();

    let fetcher = OPSuccinctDataFetcher::new();
    let headers = fetcher.get_header_preimages(&boot_infos, l1_head.into()).await?;

    let prover = NetworkProver::new();
    let (_, vkey) = prover.setup(MULTI_BLOCK_ELF);

    let stdin = get_agg_proof_stdin(proofs, boot_infos, headers, &vkey, l1_head.into()).unwrap();
    let proof_id = prover.request_proof(AGG_ELF, stdin, ProofMode::Plonk).await?;

    Ok((StatusCode::OK, Json(ProofResponse { proof_id })))
}

/// Get the status of a proof.
async fn get_proof_status(
    Path(proof_id): Path<String>,
) -> Result<(StatusCode, Json<ProofStatus>), AppError> {
    info!("Received proof status request: {:?}", proof_id);
    dotenv::dotenv().ok();
    let private_key = env::var("SP1_PRIVATE_KEY")?;

    let client = NetworkClient::new(&private_key);

    // Time out this request if it takes too long.
    let timeout = Duration::from_secs(10);
    let (status, maybe_proof) = tokio::time::timeout(timeout, client.get_proof_status(&proof_id))
        .await
        .map_err(|_| AppError(anyhow::anyhow!("Proof status request timed out")))?
        .map_err(|e| AppError(anyhow::anyhow!("Failed to get proof status: {}", e)))?;

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
        Json(ProofStatus { status: status.as_str_name().to_string(), proof: vec![] }),
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

/// Deserialize a vector of base64 strings into a vector of vectors of bytes. Go serializes
/// the subproofs as base64 strings.
fn deserialize_base64_vec<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Vec<String> = Deserialize::deserialize(deserializer)?;
    s.into_iter()
        .map(|base64_str| {
            general_purpose::STANDARD.decode(base64_str).map_err(serde::de::Error::custom)
        })
        .collect()
}
