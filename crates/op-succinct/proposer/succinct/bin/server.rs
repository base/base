use alloy_primitives::{hex, Address, B256};
use anyhow::Result;
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use log::{error, info};
use op_succinct_client_utils::{
    boot::{hash_rollup_config, BootInfoStruct},
    types::u32_to_u8,
};
use op_succinct_host_utils::{
    fetcher::{CacheMode, OPSuccinctDataFetcher, RunContext},
    get_agg_proof_stdin, get_proof_stdin,
    witnessgen::{WitnessGenExecutor, WITNESSGEN_TIMEOUT},
    L2OutputOracle, ProgramType,
};
use op_succinct_proposer::{
    AggProofRequest, ContractConfig, ProofResponse, ProofStatus, SpanProofRequest,
    ValidateConfigRequest, ValidateConfigResponse,
};
use sp1_sdk::{
    network::{
        proto::network::{ExecutionStatus, FulfillmentStatus},
        FulfillmentStrategy,
    },
    utils, HashableKey, Prover, ProverClient, SP1Proof, SP1ProofWithPublicValues,
};
use std::{env, str::FromStr, time::Duration};
use tower_http::limit::RequestBodyLimitLayer;

pub const RANGE_ELF: &[u8] = include_bytes!("../../../elf/range-elf");
pub const AGG_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");

#[tokio::main]
async fn main() -> Result<()> {
    // Set up the SP1 SDK logger.
    utils::setup_logger();

    // Enable logging.
    env::set_var("RUST_LOG", "info");

    dotenv::dotenv().ok();

    let prover = ProverClient::builder().mock().build();
    let (range_pk, range_vk) = prover.setup(RANGE_ELF);
    let (agg_pk, agg_vk) = prover.setup(AGG_ELF);
    let multi_block_vkey_u8 = u32_to_u8(range_vk.vk.hash_u32());
    let range_vkey_commitment = B256::from(multi_block_vkey_u8);
    let agg_vkey_hash = B256::from_str(&agg_vk.bytes32()).unwrap();

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Docker).await?;
    // Note: The rollup config hash never changes for a given chain, so we can just hash it once at
    // server start-up. The only time a rollup config changes is typically when a new version of the
    // [`RollupConfig`] is released from `op-alloy`.
    let rollup_config_hash = hash_rollup_config(fetcher.rollup_config.as_ref().unwrap());

    // Initialize global hashes.
    let global_hashes = ContractConfig {
        agg_vkey_hash,
        range_vkey_commitment,
        rollup_config_hash,
        range_vk,
        range_pk,
        agg_vk,
        agg_pk,
    };

    let app = Router::new()
        .route("/request_span_proof", post(request_span_proof))
        .route("/request_agg_proof", post(request_agg_proof))
        .route("/request_mock_span_proof", post(request_mock_span_proof))
        .route("/request_mock_agg_proof", post(request_mock_agg_proof))
        .route("/status/:proof_id", get(get_proof_status))
        .route("/validate_config", post(validate_config))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(102400 * 1024 * 1024))
        .with_state(global_hashes);

    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .unwrap();

    info!("Server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await?;
    Ok(())
}

/// Validate the configuration of the L2 Output Oracle.
async fn validate_config(
    State(state): State<ContractConfig>,
    Json(payload): Json<ValidateConfigRequest>,
) -> Result<(StatusCode, Json<ValidateConfigResponse>), AppError> {
    info!("Received validate config request: {:?}", payload);
    let fetcher = OPSuccinctDataFetcher::default();

    let address = Address::from_str(&payload.address).unwrap();
    let l2_output_oracle = L2OutputOracle::new(address, fetcher.l1_provider);

    let agg_vkey = l2_output_oracle.aggregationVkey().call().await?;
    let range_vkey = l2_output_oracle.rangeVkeyCommitment().call().await?;
    let rollup_config_hash = l2_output_oracle.rollupConfigHash().call().await?;

    let agg_vkey_valid = agg_vkey.aggregationVkey == state.agg_vkey_hash;
    let range_vkey_valid = range_vkey.rangeVkeyCommitment == state.range_vkey_commitment;
    let rollup_config_hash_valid = rollup_config_hash.rollupConfigHash == state.rollup_config_hash;

    Ok((
        StatusCode::OK,
        Json(ValidateConfigResponse {
            rollup_config_hash_valid,
            agg_vkey_valid,
            range_vkey_valid,
        }),
    ))
}

/// Request a proof for a span of blocks.
async fn request_span_proof(
    State(state): State<ContractConfig>,
    Json(payload): Json<SpanProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received span proof request: {:?}", payload);
    let fetcher = match OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Docker).await {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to create data fetcher: {}", e);
            return Err(AppError(e));
        }
    };

    let host_cli = match fetcher
        .get_host_cli_args(
            payload.start,
            payload.end,
            ProgramType::Multi,
            CacheMode::DeleteCache,
        )
        .await
    {
        Ok(cli) => cli,
        Err(e) => {
            error!("Failed to get host CLI args: {}", e);
            return Err(AppError(anyhow::anyhow!(
                "Failed to get host CLI args: {}",
                e
            )));
        }
    };

    // Start the server and native client with a timeout.
    // Note: Ideally, the server should call out to a separate process that executes the native
    // host, and return an ID that the client can poll on to check if the proof was submitted.
    let mut witnessgen_executor = WitnessGenExecutor::new(WITNESSGEN_TIMEOUT, RunContext::Docker);
    if let Err(e) = witnessgen_executor.spawn_witnessgen(&host_cli).await {
        error!("Failed to spawn witness generation: {}", e);
        return Err(AppError(anyhow::anyhow!(
            "Failed to spawn witness generation: {}",
            e
        )));
    }
    // Log any errors from running the witness generation process.
    if let Err(e) = witnessgen_executor.flush().await {
        error!("Failed to generate witness: {}", e);
        return Err(AppError(anyhow::anyhow!(
            "Failed to generate witness: {}",
            e
        )));
    }

    let sp1_stdin = match get_proof_stdin(&host_cli) {
        Ok(stdin) => stdin,
        Err(e) => {
            error!("Failed to get proof stdin: {}", e);
            return Err(AppError(anyhow::anyhow!(
                "Failed to get proof stdin: {}",
                e
            )));
        }
    };

    let client = ProverClient::builder().network().build();
    let proof_id = client
        .prove(&state.range_pk, &sp1_stdin)
        .compressed()
        .strategy(FulfillmentStrategy::Reserved)
        .skip_simulation(true)
        .cycle_limit(1_000_000_000_000)
        .request_async()
        .await
        .map_err(|e| {
            error!("Failed to request proof: {}", e);
            AppError(anyhow::anyhow!("Failed to request proof: {}", e))
        })?;

    Ok((
        StatusCode::OK,
        Json(ProofResponse {
            proof_id: proof_id.to_vec(),
        }),
    ))
}

/// Request an aggregation proof for a set of subproofs.
async fn request_agg_proof(
    State(state): State<ContractConfig>,
    Json(payload): Json<AggProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received agg proof request");
    let mut proofs_with_pv: Vec<SP1ProofWithPublicValues> = payload
        .subproofs
        .iter()
        .map(|sp| bincode::deserialize(sp).unwrap())
        .collect();

    let boot_infos: Vec<BootInfoStruct> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.public_values.read())
        .collect();

    let proofs: Vec<SP1Proof> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.proof.clone())
        .collect();

    let l1_head_bytes = match payload.head.strip_prefix("0x") {
        Some(hex_str) => match hex::decode(hex_str) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to decode L1 head hex string: {}", e);
                return Err(AppError(anyhow::anyhow!(
                    "Failed to decode L1 head hex string: {}",
                    e
                )));
            }
        },
        None => {
            error!("Invalid L1 head format: missing 0x prefix");
            return Err(AppError(anyhow::anyhow!(
                "Invalid L1 head format: missing 0x prefix"
            )));
        }
    };

    let l1_head: [u8; 32] = match l1_head_bytes.clone().try_into() {
        Ok(array) => array,
        Err(_) => {
            error!(
                "Invalid L1 head length: expected 32 bytes, got {}",
                l1_head_bytes.len()
            );
            return Err(AppError(anyhow::anyhow!(
                "Invalid L1 head length: expected 32 bytes, got {}",
                l1_head_bytes.len()
            )));
        }
    };

    let fetcher = match OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Docker).await {
        Ok(f) => f,
        Err(e) => return Err(AppError(anyhow::anyhow!("Failed to create fetcher: {}", e))),
    };

    let headers = match fetcher
        .get_header_preimages(&boot_infos, l1_head.into())
        .await
    {
        Ok(h) => h,
        Err(e) => {
            error!("Failed to get header preimages: {}", e);
            return Err(AppError(anyhow::anyhow!(
                "Failed to get header preimages: {}",
                e
            )));
        }
    };

    let prover = ProverClient::builder().network().build();

    let stdin =
        match get_agg_proof_stdin(proofs, boot_infos, headers, &state.range_vk, l1_head.into()) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to get agg proof stdin: {}", e);
                return Err(AppError(anyhow::anyhow!(
                    "Failed to get agg proof stdin: {}",
                    e
                )));
            }
        };

    let proof_id = match prover
        .prove(&state.agg_pk, &stdin)
        .groth16()
        .strategy(FulfillmentStrategy::Reserved)
        .request_async()
        .await
    {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to request proof: {}", e);
            return Err(AppError(anyhow::anyhow!("Failed to request proof: {}", e)));
        }
    };

    Ok((
        StatusCode::OK,
        Json(ProofResponse {
            proof_id: proof_id.to_vec(),
        }),
    ))
}

/// Request a mock proof for a span of blocks.
async fn request_mock_span_proof(
    State(state): State<ContractConfig>,
    Json(payload): Json<SpanProofRequest>,
) -> Result<(StatusCode, Json<ProofStatus>), AppError> {
    info!("Received mock span proof request: {:?}", payload);
    let fetcher = match OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Docker).await {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to create data fetcher: {}", e);
            return Err(AppError(e));
        }
    };

    let host_cli = match fetcher
        .get_host_cli_args(
            payload.start,
            payload.end,
            ProgramType::Multi,
            CacheMode::DeleteCache,
        )
        .await
    {
        Ok(cli) => cli,
        Err(e) => {
            error!("Failed to get host CLI args: {}", e);
            return Err(AppError(e));
        }
    };

    // Start the server and native client with a timeout.
    // Note: Ideally, the server should call out to a separate process that executes the native
    // host, and return an ID that the client can poll on to check if the proof was submitted.
    let mut witnessgen_executor = WitnessGenExecutor::new(WITNESSGEN_TIMEOUT, RunContext::Docker);
    if let Err(e) = witnessgen_executor.spawn_witnessgen(&host_cli).await {
        error!("Failed to spawn witness generator: {}", e);
        return Err(AppError(e));
    }
    // Log any errors from running the witness generation process.
    if let Err(e) = witnessgen_executor.flush().await {
        error!("Failed to generate witness: {}", e);
        return Err(AppError(anyhow::anyhow!(
            "Failed to generate witness: {}",
            e
        )));
    }

    let sp1_stdin = match get_proof_stdin(&host_cli) {
        Ok(stdin) => stdin,
        Err(e) => {
            error!("Failed to get proof stdin: {}", e);
            return Err(AppError(e));
        }
    };

    let prover = ProverClient::builder().mock().build();
    let proof = prover
        .prove(&state.range_pk, &sp1_stdin)
        .compressed()
        .run()?;

    let proof_bytes = bincode::serialize(&proof).unwrap();

    Ok((
        StatusCode::OK,
        Json(ProofStatus {
            fulfillment_status: FulfillmentStatus::Fulfilled.into(),
            execution_status: ExecutionStatus::UnspecifiedExecutionStatus.into(),
            proof: proof_bytes,
        }),
    ))
}

/// Request mock aggregation proof.
async fn request_mock_agg_proof(
    State(state): State<ContractConfig>,
    Json(payload): Json<AggProofRequest>,
) -> Result<(StatusCode, Json<ProofStatus>), AppError> {
    info!("Received mock agg proof request!");

    let mut proofs_with_pv: Vec<SP1ProofWithPublicValues> = payload
        .subproofs
        .iter()
        .map(|sp| bincode::deserialize(sp).unwrap())
        .collect();

    let boot_infos: Vec<BootInfoStruct> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.public_values.read())
        .collect();

    let proofs: Vec<SP1Proof> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.proof.clone())
        .collect();

    let l1_head_bytes = match hex::decode(
        payload
            .head
            .strip_prefix("0x")
            .expect("Invalid L1 head, no 0x prefix."),
    ) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to decode L1 head: {}", e);
            return Err(AppError(anyhow::anyhow!("Failed to decode L1 head: {}", e)));
        }
    };
    let l1_head: [u8; 32] = l1_head_bytes.try_into().unwrap();

    let fetcher = match OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Docker).await {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to create data fetcher: {}", e);
            return Err(AppError(e));
        }
    };
    let headers = match fetcher
        .get_header_preimages(&boot_infos, l1_head.into())
        .await
    {
        Ok(h) => h,
        Err(e) => {
            error!("Failed to get header preimages: {}", e);
            return Err(AppError(e));
        }
    };

    let prover = ProverClient::builder().mock().build();

    let stdin =
        match get_agg_proof_stdin(proofs, boot_infos, headers, &state.range_vk, l1_head.into()) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to get aggregation proof stdin: {}", e);
                return Err(AppError(e));
            }
        };

    let proof = match prover
        .prove(&state.agg_pk, &stdin)
        .groth16()
        .deferred_proof_verification(false)
        .run()
    {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to generate proof: {}", e);
            return Err(AppError(e));
        }
    };

    Ok((
        StatusCode::OK,
        Json(ProofStatus {
            fulfillment_status: FulfillmentStatus::Fulfilled.into(),
            execution_status: ExecutionStatus::UnspecifiedExecutionStatus.into(),
            proof: proof.bytes(),
        }),
    ))
}

/// Get the status of a proof.
async fn get_proof_status(
    Path(proof_id): Path<String>,
) -> Result<(StatusCode, Json<ProofStatus>), AppError> {
    info!("Received proof status request: {:?}", proof_id);

    let client = ProverClient::builder().network().build();

    let proof_id_bytes = hex::decode(proof_id)?;

    // Time out this request if it takes too long.
    let timeout = Duration::from_secs(10);
    let (status, maybe_proof) = match tokio::time::timeout(
        timeout,
        client.get_proof_status(B256::from_slice(&proof_id_bytes)),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProofStatus {
                    fulfillment_status: FulfillmentStatus::UnspecifiedFulfillmentStatus.into(),
                    execution_status: ExecutionStatus::UnspecifiedExecutionStatus.into(),
                    proof: vec![],
                }),
            ));
        }
        Err(_) => {
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProofStatus {
                    fulfillment_status: FulfillmentStatus::UnspecifiedFulfillmentStatus.into(),
                    execution_status: ExecutionStatus::UnspecifiedExecutionStatus.into(),
                    proof: vec![],
                }),
            ));
        }
    };

    let fulfillment_status = status.fulfillment_status;
    let execution_status = status.execution_status;
    if fulfillment_status == FulfillmentStatus::Fulfilled as i32 {
        let proof: SP1ProofWithPublicValues = maybe_proof.unwrap();

        match proof.proof {
            SP1Proof::Compressed(_) => {
                // If it's a compressed proof, we need to serialize the entire struct with bincode.
                // Note: We're re-serializing the entire struct with bincode here, but this is fine
                // because we're on localhost and the size of the struct is small.
                let proof_bytes = bincode::serialize(&proof).unwrap();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        fulfillment_status,
                        execution_status,
                        proof: proof_bytes,
                    }),
                ));
            }
            SP1Proof::Groth16(_) => {
                // If it's a groth16 proof, we need to get the proof bytes that we put on-chain.
                let proof_bytes = proof.bytes();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        fulfillment_status,
                        execution_status,
                        proof: proof_bytes,
                    }),
                ));
            }
            SP1Proof::Plonk(_) => {
                // If it's a plonk proof, we need to get the proof bytes that we put on-chain.
                let proof_bytes = proof.bytes();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        fulfillment_status,
                        execution_status,
                        proof: proof_bytes,
                    }),
                ));
            }
            _ => (),
        }
    } else if fulfillment_status == FulfillmentStatus::Unfulfillable as i32 {
        return Ok((
            StatusCode::OK,
            Json(ProofStatus {
                fulfillment_status,
                execution_status,
                proof: vec![],
            }),
        ));
    }
    Ok((
        StatusCode::OK,
        Json(ProofStatus {
            fulfillment_status,
            execution_status,
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
