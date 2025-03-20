use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use anyhow::{Context, Result};
use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin, get_proof_stdin, hosts::OPSuccinctHost,
    metrics::MetricsGauge, AGGREGATION_ELF, RANGE_ELF_EMBEDDED,
};
use sp1_sdk::{
    network::{proto::network::ExecutionStatus, FulfillmentStrategy},
    NetworkProver, SP1Proof, SP1ProofMode, SP1ProofWithPublicValues, SP1Stdin, SP1_CIRCUIT_VERSION,
};
use std::{sync::Arc, time::Instant};
use tracing::{info, warn};

use crate::db::DriverDBClient;
use crate::{
    OPSuccinctRequest, ProgramConfig, RequestExecutionStatistics, RequestStatus, RequestType,
    ValidityGauge,
};

pub struct OPSuccinctProofRequester<H: OPSuccinctHost> {
    pub host: Arc<H>,
    pub network_prover: Arc<NetworkProver>,
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub db_client: Arc<DriverDBClient>,
    pub program_config: ProgramConfig,
    pub mock: bool,
    pub range_strategy: FulfillmentStrategy,
    pub agg_strategy: FulfillmentStrategy,
    pub agg_mode: SP1ProofMode,
    pub safe_db_fallback: bool,
}

impl<H: OPSuccinctHost> OPSuccinctProofRequester<H> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: Arc<H>,
        network_prover: Arc<NetworkProver>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        db_client: Arc<DriverDBClient>,
        program_config: ProgramConfig,
        mock: bool,
        range_strategy: FulfillmentStrategy,
        agg_strategy: FulfillmentStrategy,
        agg_mode: SP1ProofMode,
        safe_db_fallback: bool,
    ) -> Self {
        Self {
            host,
            network_prover,
            fetcher,
            db_client,
            program_config,
            mock,
            range_strategy,
            agg_strategy,
            agg_mode,
            safe_db_fallback,
        }
    }

    /// Generates the witness for a range proof.
    pub async fn range_proof_witnessgen(&self, request: &OPSuccinctRequest) -> Result<SP1Stdin> {
        let host_args = self
            .host
            .fetch(
                request.start_block as u64,
                request.end_block as u64,
                None,
                Some(self.safe_db_fallback),
            )
            .await?;

        if let Some(l1_head) = self.host.get_l1_head_hash(&host_args) {
            let l1_head_block_number = self.fetcher.get_l1_header(l1_head.into()).await?.number;
            self.db_client
                .update_l1_head_block_number(request.id, l1_head_block_number as i64)
                .await?;
        }

        let mem_kv_store = self.host.run(&host_args).await?;
        let sp1_stdin = get_proof_stdin(mem_kv_store).context("Failed to get proof stdin")?;

        Ok(sp1_stdin)
    }

    /// Generates the witness for an aggregation proof.
    pub async fn agg_proof_witnessgen(
        &self,
        start_block: i64,
        end_block: i64,
        checkpointed_l1_block_hash: B256,
        l1_chain_id: i64,
        l2_chain_id: i64,
        prover_address: Address,
    ) -> Result<SP1Stdin> {
        // Fetch consecutive range proofs from the database.
        let range_proofs = self
            .db_client
            .get_consecutive_complete_range_proofs(
                start_block,
                end_block,
                &self.program_config.commitments,
                l1_chain_id,
                l2_chain_id,
            )
            .await?;

        // Deserialize the proofs and extract the boot infos and proofs.
        let (boot_infos, proofs): (Vec<BootInfoStruct>, Vec<SP1Proof>) = range_proofs
            .iter()
            .map(|proof| {
                let mut proof_with_pv: SP1ProofWithPublicValues =
                    bincode::deserialize(proof.proof.as_ref().unwrap())
                        .expect("Deserialization failure for range proof");
                (
                    proof_with_pv.public_values.read(),
                    proof_with_pv.proof.clone(),
                )
            })
            .unzip();

        // This can fail for a few reasons:
        // 1. The L1 RPC is down (e.g. error code 32001). Double-check the L1 RPC is running correctly.
        // 2. The L1 head was re-orged and the block is no longer available. This is unlikely given we wait for 3 confirmations on a transaction.
        let headers = self
            .fetcher
            .get_header_preimages(&boot_infos, checkpointed_l1_block_hash)
            .await
            .context("Failed to get header preimages")?;

        let stdin = get_agg_proof_stdin(
            proofs,
            boot_infos,
            headers,
            &self.program_config.range_vk,
            checkpointed_l1_block_hash,
            prover_address,
        )
        .context("Failed to get agg proof stdin")?;

        Ok(stdin)
    }

    /// Requests a range proof via the network prover.
    pub async fn request_range_proof(&self, stdin: SP1Stdin) -> Result<B256> {
        let proof_id = match self
            .network_prover
            .prove(&self.program_config.range_pk, &stdin)
            .compressed()
            .strategy(self.range_strategy)
            .skip_simulation(true)
            .cycle_limit(1_000_000_000_000)
            .request_async()
            .await
        {
            Ok(proof_id) => proof_id,
            Err(e) => {
                ValidityGauge::RangeProofRequestErrorCount.increment(1.0);
                return Err(e);
            }
        };

        Ok(proof_id)
    }

    /// Requests an aggregation proof via the network prover.
    pub async fn request_agg_proof(&self, stdin: SP1Stdin) -> Result<B256> {
        let proof_id = match self
            .network_prover
            .prove(&self.program_config.agg_pk, &stdin)
            .mode(self.agg_mode)
            .strategy(self.agg_strategy)
            .request_async()
            .await
        {
            Ok(proof_id) => proof_id,
            Err(e) => {
                ValidityGauge::AggProofRequestErrorCount.increment(1.0);
                return Err(e);
            }
        };

        Ok(proof_id)
    }

    /// Generates a mock range proof and writes the execution statistics to the database.
    pub async fn generate_mock_range_proof(
        &self,
        request: &OPSuccinctRequest,
        stdin: SP1Stdin,
    ) -> Result<SP1ProofWithPublicValues> {
        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            "Executing mock range proof"
        );

        let start_time = Instant::now();
        let network_prover = self.network_prover.clone();
        // Move the CPU-intensive operation to a dedicated thread.
        let (pv, report) = match tokio::task::spawn_blocking(move || {
            network_prover.execute(RANGE_ELF_EMBEDDED, &stdin).run()
        })
        .await?
        {
            Ok((pv, report)) => (pv, report),
            Err(e) => {
                ValidityGauge::ExecutionErrorCount.increment(1.0);
                return Err(e);
            }
        };

        let execution_duration = start_time.elapsed().as_secs();

        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            duration_s = execution_duration,
            "Executed mock range proof.",
        );

        let execution_statistics = RequestExecutionStatistics::new(report);

        // Write the execution data to the database.
        self.db_client
            .insert_execution_statistics(
                request.id,
                serde_json::to_value(execution_statistics)?,
                execution_duration as i64,
            )
            .await?;

        Ok(SP1ProofWithPublicValues::create_mock_proof(
            &self.program_config.range_pk,
            pv.clone(),
            SP1ProofMode::Compressed,
            SP1_CIRCUIT_VERSION,
        ))
    }

    /// Generates a mock aggregation proof.
    pub async fn generate_mock_agg_proof(
        &self,
        request: &OPSuccinctRequest,
        stdin: SP1Stdin,
    ) -> Result<SP1ProofWithPublicValues> {
        let start_time = Instant::now();
        let network_prover = self.network_prover.clone();
        // Move the CPU-intensive operation to a dedicated thread.
        let (pv, report) = match tokio::task::spawn_blocking(move || {
            network_prover
                .execute(AGGREGATION_ELF, &stdin)
                .deferred_proof_verification(false)
                .run()
        })
        .await?
        {
            Ok((pv, report)) => (pv, report),
            Err(e) => {
                ValidityGauge::ExecutionErrorCount.increment(1.0);
                return Err(e);
            }
        };

        let execution_duration = start_time.elapsed().as_secs();

        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            duration_s = execution_duration,
            "Executed mock aggregation proof.",
        );

        let execution_statistics = RequestExecutionStatistics::new(report);

        // Write the execution data to the database.
        self.db_client
            .insert_execution_statistics(
                request.id,
                serde_json::to_value(execution_statistics)?,
                execution_duration as i64,
            )
            .await?;

        Ok(SP1ProofWithPublicValues::create_mock_proof(
            &self.program_config.agg_pk,
            pv.clone(),
            self.agg_mode,
            SP1_CIRCUIT_VERSION,
        ))
    }

    /// Handles a failed proof request.
    ///
    /// If the request is a range proof and the number of failed requests is greater than 2 or the execution status is unexecutable, the request is split into two new requests.
    /// Otherwise, add_new_ranges will insert the new request. This ensures better failure-resilience. If the request to add two range requests fails, add_new_ranges will handle it gracefully by submitting
    /// the same range.
    #[tracing::instrument(
        name = "proof_requester.handle_failed_request",
        skip(self, request, execution_status)
    )]
    pub async fn handle_failed_request(
        &self,
        request: OPSuccinctRequest,
        execution_status: ExecutionStatus,
    ) -> Result<()> {
        warn!(
            id = request.id,
            start_block = request.start_block,
            end_block = request.end_block,
            req_type = ?request.req_type,
            "Setting request to failed"
        );

        // Mark the existing request as failed.
        self.db_client
            .update_request_status(request.id, RequestStatus::Failed)
            .await?;

        let l1_chain_id = self.fetcher.l1_provider.get_chain_id().await?;
        let l2_chain_id = self.fetcher.l2_provider.get_chain_id().await?;

        if request.end_block - request.start_block > 1 && request.req_type == RequestType::Range {
            let num_failed_requests = self
                .db_client
                .fetch_failed_request_count_by_block_range(
                    request.start_block,
                    request.end_block,
                    request.l1_chain_id,
                    request.l2_chain_id,
                    &self.program_config.commitments,
                )
                .await?;

            // NOTE: The failed_requests check here can be removed in V5.
            if num_failed_requests > 2 || execution_status == ExecutionStatus::Unexecutable {
                info!("Splitting failed request into two: {:?}", request.id);
                let mid_block = (request.start_block + request.end_block) / 2;
                let new_requests = vec![
                    OPSuccinctRequest::create_range_request(
                        request.mode,
                        request.start_block,
                        mid_block,
                        self.program_config.commitments.range_vkey_commitment,
                        self.program_config.commitments.rollup_config_hash,
                        l1_chain_id as i64,
                        l2_chain_id as i64,
                        self.fetcher.clone(),
                    )
                    .await?,
                    OPSuccinctRequest::create_range_request(
                        request.mode,
                        mid_block,
                        request.end_block,
                        self.program_config.commitments.range_vkey_commitment,
                        self.program_config.commitments.rollup_config_hash,
                        l1_chain_id as i64,
                        l2_chain_id as i64,
                        self.fetcher.clone(),
                    )
                    .await?,
                ];

                self.db_client.insert_requests(&new_requests).await?;
            }
        }

        Ok(())
    }

    /// Generates the stdin needed for a proof.
    async fn generate_proof_stdin(&self, request: &OPSuccinctRequest) -> Result<SP1Stdin> {
        let stdin = match request.req_type {
            RequestType::Range => self.range_proof_witnessgen(request).await?,
            RequestType::Aggregation => {
                self.agg_proof_witnessgen(
                    request.start_block,
                    request.end_block,
                    B256::from_slice(request.checkpointed_l1_block_hash.as_ref().ok_or_else(
                        || anyhow::anyhow!("Aggregation proof has no checkpointed block."),
                    )?),
                    request.l1_chain_id,
                    request.l2_chain_id,
                    Address::from_slice(request.prover_address.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Prover address must be set for aggregation proofs.")
                    })?),
                )
                .await?
            }
        };

        Ok(stdin)
    }

    /// Makes a proof request by updating statuses, generating witnesses, and then either requesting or
    /// mocking the proof depending on configuration.
    ///
    /// Note: Any error from this function will cause the proof to be retried.
    #[tracing::instrument(name = "proof_requester.make_proof_request", skip(self, request))]
    pub async fn make_proof_request(&self, request: OPSuccinctRequest) -> Result<()> {
        // Update status to WitnessGeneration.
        self.db_client
            .update_request_status(request.id, RequestStatus::WitnessGeneration)
            .await?;

        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            "Starting witness generation"
        );

        let witnessgen_duration = Instant::now();
        // Generate the stdin needed for the proof. If this fails, retry the request.
        let stdin = match self.generate_proof_stdin(&request).await {
            Ok(stdin) => stdin,
            Err(e) => {
                ValidityGauge::WitnessgenErrorCount.increment(1.0);
                return Err(e);
            }
        };
        let duration = witnessgen_duration.elapsed();

        self.db_client
            .update_witnessgen_duration(request.id, duration.as_secs() as i64)
            .await?;

        info!(
            request_id = request.id,
            start_block = request.start_block,
            end_block = request.end_block,
            request_type = ?request.req_type,
            duration_s = duration.as_secs(),
            "Completed witness generation"
        );

        // For mock mode, update status to Execution before proceeding.
        if self.mock {
            self.db_client
                .update_request_status(request.id, RequestStatus::Execution)
                .await?;
        }

        match request.req_type {
            RequestType::Range => {
                if self.mock {
                    let proof = self.generate_mock_range_proof(&request, stdin).await?;
                    let proof_bytes = bincode::serialize(&proof).unwrap();
                    self.db_client
                        .update_proof_to_complete(request.id, &proof_bytes)
                        .await?;
                } else {
                    let proof_id = self.request_range_proof(stdin).await?;
                    self.db_client
                        .update_request_to_prove(request.id, proof_id)
                        .await?;
                }
            }
            RequestType::Aggregation => {
                if self.mock {
                    let proof = self.generate_mock_agg_proof(&request, stdin).await?;
                    self.db_client
                        .update_proof_to_complete(request.id, &proof.bytes())
                        .await?;
                } else {
                    let proof_id = self.request_agg_proof(stdin).await?;
                    self.db_client
                        .update_request_to_prove(request.id, proof_id)
                        .await?;
                }
            }
        }

        Ok(())
    }
}
