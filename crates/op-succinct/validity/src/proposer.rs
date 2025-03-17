use crate::{
    db::{DriverDBClient, OPSuccinctRequest, RequestMode, RequestStatus},
    find_gaps, get_latest_proposed_block_number, get_ranges_to_prove, init_gauges, GaugeMetric,
    OPSuccinctProofRequester,
};
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{network::ReceiptResponse, Network, Provider};
use alloy_sol_types::SolValue;
use anyhow::anyhow;
use anyhow::{Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, hosts::OPSuccinctHost,
    DisputeGameFactory::DisputeGameFactoryInstance as DisputeGameFactoryContract,
    OPSuccinctL2OutputOracle::OPSuccinctL2OutputOracleInstance as OPSuccinctL2OOContract,
    ValidityDisputeGameExtraData, AGGREGATION_ELF, RANGE_ELF_EMBEDDED,
};
use sp1_sdk::{
    network::{
        proto::network::{ExecutionStatus, FulfillmentStatus},
        FulfillmentStrategy,
    },
    HashableKey, NetworkProver, Prover, ProverClient, SP1Proof, SP1ProofMode,
    SP1ProofWithPublicValues, SP1ProvingKey, SP1VerifyingKey,
};
use std::collections::HashMap;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

pub struct ContractConfig<P, N>
where
    P: Provider<N> + 'static,
    N: Network,
{
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub l2oo_contract: OPSuccinctL2OOContract<(), P, N>,
    pub dgf_contract: DisputeGameFactoryContract<(), P, N>,
}

#[derive(Debug, Clone)]
pub struct CommitmentConfig {
    pub range_vkey_commitment: B256,
    pub agg_vkey_hash: B256,
    pub rollup_config_hash: B256,
}

#[derive(Clone)]
pub struct ProgramConfig {
    pub range_vk: Arc<SP1VerifyingKey>,
    pub range_pk: Arc<SP1ProvingKey>,
    pub agg_vk: Arc<SP1VerifyingKey>,
    pub agg_pk: Arc<SP1ProvingKey>,
    pub commitments: CommitmentConfig,
}

pub struct RequesterConfig {
    pub l1_chain_id: i64,
    pub l2_chain_id: i64,
    // The address being committed to when generating the aggregation proof to prevent front-running attacks.
    // This should be the same address that is being used to send `proposeL2Output` transactions.
    pub prover_address: Address,
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub range_proof_interval: u64,
    pub submission_interval: u64,
    pub max_concurrent_witness_gen: u64,
    pub max_concurrent_proof_requests: u64,
    pub range_proof_strategy: FulfillmentStrategy,
    pub agg_proof_strategy: FulfillmentStrategy,
    pub agg_proof_mode: SP1ProofMode,
    pub mock: bool,
    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not activated for op-node.
    pub safe_db_fallback: bool,
}

/// Configuration for the driver.
pub struct DriverConfig {
    pub network_prover: Arc<NetworkProver>,
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub driver_db_client: Arc<DriverDBClient>,
    pub loop_interval_seconds: u64,
}
/// Type alias for a map of task IDs to their join handles and associated requests
// TODO: Investigate whether we can use DashMap for this.
pub type TaskMap =
    HashMap<tokio::task::Id, (tokio::task::JoinHandle<Result<()>>, OPSuccinctRequest)>;

pub struct Proposer<P, N, H: OPSuccinctHost>
where
    P: Provider<N> + 'static,
    N: Network,
{
    driver_config: DriverConfig,
    contract_config: ContractConfig<P, N>,
    program_config: ProgramConfig,
    requester_config: RequesterConfig,
    proof_requester: Arc<OPSuccinctProofRequester<H>>,
    tasks: Arc<Mutex<TaskMap>>,
}

/// 5 L1 confirmations (1 minute)
const NUM_CONFIRMATIONS: u64 = 5;
/// 2 minute timeout.
const TIMEOUT: u64 = 120;
/// Task completion handler interval.
const TASK_COMPLETION_HANDLER_INTERVAL: u64 = 1;

impl<P, N, H: OPSuccinctHost> Proposer<P, N, H>
where
    P: Provider<N> + 'static + Clone,
    N: Network,
{
    pub async fn new(
        provider: P,
        db_client: Arc<DriverDBClient>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        requester_config: RequesterConfig,
        loop_interval_seconds: Option<u64>,
        host: Arc<H>,
    ) -> Result<Self> {
        let network_prover = Arc::new(ProverClient::builder().network().build());
        let (range_pk, range_vk) = network_prover.setup(RANGE_ELF_EMBEDDED);
        let (agg_pk, agg_vk) = network_prover.setup(AGGREGATION_ELF);
        let multi_block_vkey_u8 = u32_to_u8(range_vk.vk.hash_u32());
        let range_vkey_commitment = B256::from(multi_block_vkey_u8);
        let agg_vkey_hash = B256::from_str(&agg_vk.bytes32()).unwrap();

        // Initialize fetcher
        let rollup_config_hash = hash_rollup_config(fetcher.rollup_config.as_ref().unwrap());

        let program_config = ProgramConfig {
            range_vk: Arc::new(range_vk),
            range_pk: Arc::new(range_pk),
            agg_vk: Arc::new(agg_vk),
            agg_pk: Arc::new(agg_pk),
            commitments: CommitmentConfig {
                range_vkey_commitment,
                agg_vkey_hash,
                rollup_config_hash,
            },
        };

        // Initialize the proof requester.
        let proof_requester = Arc::new(OPSuccinctProofRequester::new(
            host,
            network_prover.clone(),
            fetcher.clone(),
            db_client.clone(),
            program_config.clone(),
            requester_config.mock,
            requester_config.range_proof_strategy,
            requester_config.agg_proof_strategy,
            requester_config.agg_proof_mode,
            requester_config.safe_db_fallback,
        ));

        let l2oo_contract =
            OPSuccinctL2OOContract::new(requester_config.l2oo_address, provider.clone());

        let dgf_contract = DisputeGameFactoryContract::new(requester_config.dgf_address, provider);

        // 1 minute default loop interval.
        const DEFAULT_LOOP_INTERVAL: u64 = 60;

        let proposer = Proposer {
            driver_config: DriverConfig {
                network_prover,
                fetcher,
                driver_db_client: db_client,
                loop_interval_seconds: loop_interval_seconds.unwrap_or(DEFAULT_LOOP_INTERVAL),
            },
            contract_config: ContractConfig {
                l2oo_address: requester_config.l2oo_address,
                dgf_address: requester_config.dgf_address,
                l2oo_contract,
                dgf_contract,
            },
            program_config,
            requester_config,
            proof_requester,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        };
        Ok(proposer)
    }

    /// Use the in-memory index of the highest block number to add new ranges to the database.
    #[tracing::instrument(name = "proposer.add_new_ranges", skip(self))]
    pub async fn add_new_ranges(&self) -> Result<()> {
        let latest_finalized_header = self
            .driver_config
            .fetcher
            .get_l2_header(BlockId::finalized())
            .await?;

        // Get the highest block number of any of range request in the database that is not FAILED or CANCELLED or RELAYED with the same commitment.
        let highest_end_block = self
            .driver_config
            .driver_db_client
            .fetch_highest_end_block_for_range_request(
                &[
                    RequestStatus::Unrequested,
                    RequestStatus::WitnessGeneration,
                    RequestStatus::Execution,
                    RequestStatus::Complete,
                    RequestStatus::Prove,
                ],
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        // If there are no requests in the database, the current processed block number is the latest finalized block number on the contract. Otherwise, it's the highest block number
        // of any of the requests in the database that are not FAILED or CANCELLED.
        let mut current_processed_block = match highest_end_block {
            Some(end_block) => end_block as u64,
            None => {
                tracing::debug!(
                    "No requests in the database, using latest proposed block number on contract."
                );
                get_latest_proposed_block_number(
                    self.contract_config.l2oo_address,
                    self.driver_config.fetcher.as_ref(),
                )
                .await?
            }
        };
        tracing::debug!("Current processed block: {}.", current_processed_block);

        let mut requests = Vec::new();

        // Only add new ranges if the current block is less than the latest finalized block minus the
        // range proof interval. This ensures that only ranges that cover range_proof_interval blocks
        // are added to the database.
        while current_processed_block
            < latest_finalized_header.number - self.requester_config.range_proof_interval
        {
            let end_block = std::cmp::min(
                current_processed_block + self.requester_config.range_proof_interval,
                latest_finalized_header.number,
            );

            let request = OPSuccinctRequest::create_range_request(
                if self.requester_config.mock {
                    RequestMode::Mock
                } else {
                    RequestMode::Real
                },
                current_processed_block as i64,
                end_block as i64,
                self.program_config.commitments.range_vkey_commitment,
                self.program_config.commitments.rollup_config_hash,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
                self.driver_config.fetcher.clone(),
            )
            .await?;

            requests.push(request);
            current_processed_block = end_block;
        }

        if !requests.is_empty() {
            tracing::debug!("Inserting {} requests into the database.", requests.len());
            self.driver_config
                .driver_db_client
                .insert_requests(&requests)
                .await?;
        }

        Ok(())
    }

    /// Handle all proof requests in the Prove state.
    #[tracing::instrument(name = "proposer.handle_proving_requests", skip(self))]
    pub async fn handle_proving_requests(&self) -> Result<()> {
        // Get all requests from the database.
        let prove_requests = self
            .driver_config
            .driver_db_client
            .fetch_requests_by_status(
                RequestStatus::Prove,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        debug!(
            "Getting proof statuses for {} requests.",
            prove_requests.len()
        );

        // Get the proof status of all of the requests in parallel.
        futures_util::future::join_all(
            prove_requests
                .into_iter()
                .map(|request| async move { self.process_proof_request_status(request).await }),
        )
        .await;

        Ok(())
    }

    /// Process a single OP Succinct request's proof status.
    #[tracing::instrument(name = "proposer.process_proof_request_status", skip(self, request))]
    pub async fn process_proof_request_status(&self, request: OPSuccinctRequest) -> Result<()> {
        if let Some(proof_request_id) = request.proof_request_id.as_ref() {
            let (status, proof) = self
                .driver_config
                .network_prover
                .get_proof_status(B256::from_slice(proof_request_id))
                .await?;

            // Check if current time exceeds deadline. If so, the proof has timed out.
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            if current_time > status.deadline {
                match self
                    .proof_requester
                    .retry_request(request.clone(), status.execution_status())
                    .await
                {
                    Ok(_) => GaugeMetric::ProofRequestRetryCount.increment(1.0),
                    Err(e) => {
                        GaugeMetric::RetryErrorCount.increment(1.0);
                        return Err(e);
                    }
                }

                GaugeMetric::ProofRequestTimeoutErrorCount.increment(1.0);

                tracing::error!(
                    "Proof request has timed out for request id: {:?}",
                    proof_request_id
                );

                return Ok(());
            }

            // If the proof request has been fulfilled, update the request to status Complete and add the proof bytes to the database.
            if status.fulfillment_status() == FulfillmentStatus::Fulfilled {
                let proof: SP1ProofWithPublicValues = proof.unwrap();

                let proof_bytes = match proof.proof {
                    // If it's a compressed proof, serialize with bincode.
                    SP1Proof::Compressed(_) => bincode::serialize(&proof).unwrap(),
                    // If it's Groth16 or PLONK, get the on-chain proof bytes.
                    SP1Proof::Groth16(_) | SP1Proof::Plonk(_) => proof.bytes(),
                    SP1Proof::Core(_) => return Err(anyhow!("Core proofs are not supported.")),
                };

                // Add the completed proof to the database.
                self.driver_config
                    .driver_db_client
                    .update_proof_to_complete(request.id, &proof_bytes)
                    .await?;
                // Update the prove_duration based on the current time and the proof_request_time.
                self.driver_config
                    .driver_db_client
                    .update_prove_duration(request.id)
                    .await?;
            } else if status.fulfillment_status() == FulfillmentStatus::Unfulfillable {
                self.proof_requester
                    .retry_request(request, status.execution_status())
                    .await?;
                GaugeMetric::ProofRequestRetryCount.increment(1.0);
            }
        } else {
            // There should never be a proof request in Prove status without a proof request id.
            tracing::warn!(id = request.id, start_block = request.start_block, end_block = request.end_block, req_type = ?request.req_type, "Request has no proof request id");
        }

        Ok(())
    }

    /// Create aggregation proofs based on the completed range proofs. The range proofs must be contiguous and have
    /// the same range vkey commitment. Assumes that the range proof retry logic guarantees that there is not
    /// two potential contiguous chains of range proofs.
    ///
    /// Only creates an Aggregation proof if there's not an Aggregation proof in progress with the same start block.
    #[tracing::instrument(name = "proposer.create_aggregation_proofs", skip(self))]
    pub async fn create_aggregation_proofs(&self) -> Result<()> {
        // Check if there's an Aggregation proof with the same start block AND range verification key commitment AND aggregation vkey.
        // If so, return.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await? as i64;

        // Get all active Aggregation proofs with the same start block, range vkey commitment, and aggregation vkey.
        let active_agg_proofs_count = self
            .driver_config
            .driver_db_client
            .fetch_active_agg_proofs_count(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        if active_agg_proofs_count > 0 {
            tracing::debug!("There is already an Aggregation proof queued with the same start block, range vkey commitment, and aggregation vkey.");
            return Ok(());
        }

        // Get the completed range proofs with a start block greater than the latest proposed block number.
        let completed_range_proofs = self
            .driver_config
            .driver_db_client
            .fetch_completed_ranges(
                &self.program_config.commitments,
                latest_proposed_block_number as i64,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        // Get the highest block number of the completed range proofs.
        let highest_proven_contiguous_block_number =
            match self.get_highest_proven_contiguous_block(completed_range_proofs)? {
                Some(block) => block,
                None => return Ok(()), // No completed range proofs contiguous to the latest proposed block number, so no need to create an aggregation proof.
            };

        // Get the submission interval from the contract.
        let contract_submission_interval: u64 = self
            .contract_config
            .l2oo_contract
            .submissionInterval()
            .call()
            .await?
            .submissionInterval
            .try_into()
            .unwrap();

        // Use the submission interval from the contract if it's greater than the one in the proposer config.
        let submission_interval =
            contract_submission_interval.max(self.requester_config.submission_interval) as i64;

        debug!(
            "Submission interval for aggregation proof: {}.",
            submission_interval
        );

        // If the highest proven contiguous block number is greater than the latest proposed block number plus the submission interval, create an aggregation proof.
        if (highest_proven_contiguous_block_number - latest_proposed_block_number)
            >= submission_interval
        {
            // If an aggregation request with the same start block and end block and commitment config exists, there's no need to checkpoint the L1 block hash.
            // Use the existing L1 block hash from the existing request.
            let existing_request = self
                .driver_config
                .driver_db_client
                .fetch_failed_agg_request_with_checkpointed_block_hash(
                    latest_proposed_block_number,
                    highest_proven_contiguous_block_number,
                    &self.program_config.commitments,
                    self.requester_config.l1_chain_id,
                    self.requester_config.l2_chain_id,
                )
                .await?;

            // If there's an existing aggregation request with the same start block, end block, and commitment config that has a checkpointed block hash, use the existing L1 block hash and number. This is
            // likely caused by an error generating the aggregation proof, but there's no need to checkpoint the L1 block hash again.
            let (checkpointed_l1_block_hash, checkpointed_l1_block_number) = if let Some(
                existing_request,
            ) = existing_request
            {
                tracing::debug!("Found existing aggregation request with the same start block, end block, and commitment config that has a checkpointed block hash.");
                (B256::from_slice(&existing_request.0), existing_request.1)
            } else {
                // Checkpoint an L1 block hash that will be used to create the aggregation proof.
                let latest_header = self
                    .driver_config
                    .fetcher
                    .get_l1_header(BlockId::latest())
                    .await?;

                // Checkpoint the L1 block hash.
                let receipt = self
                    .contract_config
                    .l2oo_contract
                    .checkpointBlockHash(U256::from(latest_header.number))
                    .send()
                    .await?
                    .with_required_confirmations(NUM_CONFIRMATIONS)
                    .with_timeout(Some(Duration::from_secs(TIMEOUT)))
                    .get_receipt()
                    .await?;

                // If transaction reverted, log the error.
                if !receipt.status() {
                    tracing::error!("Transaction reverted: {:?}", receipt);
                }

                tracing::info!("Checkpointed L1 block number: {:?}.", latest_header.number);

                (latest_header.hash_slow(), latest_header.number as i64)
            };

            // Create an aggregation proof request to cover the range with the checkpointed L1 block hash.
            self.driver_config
                .driver_db_client
                .insert_request(&OPSuccinctRequest::new_agg_request(
                    if self.requester_config.mock {
                        RequestMode::Mock
                    } else {
                        RequestMode::Real
                    },
                    latest_proposed_block_number,
                    highest_proven_contiguous_block_number,
                    self.program_config.commitments.range_vkey_commitment,
                    self.program_config.commitments.agg_vkey_hash,
                    self.program_config.commitments.rollup_config_hash,
                    self.requester_config.l1_chain_id,
                    self.requester_config.l2_chain_id,
                    checkpointed_l1_block_number,
                    checkpointed_l1_block_hash,
                    self.requester_config.prover_address,
                ))
                .await?;
        }

        Ok(())
    }

    /// Request all unrequested proofs up to MAX_CONCURRENT_PROOF_REQUESTS. If there are already MAX_CONCURRENT_PROOF_REQUESTS proofs in WitnessGeneration, Execute, and Prove status, return.
    /// If there are already MAX_CONCURRENT_WITNESS_GEN proofs in WitnessGeneration or Execute status, return.
    ///
    /// Note: In the future, submit up to MAX_CONCURRENT_PROOF_REQUESTS at a time. Don't do one per loop.
    #[tracing::instrument(name = "proposer.request_queued_proofs", skip(self))]
    async fn request_queued_proofs(&self) -> Result<()> {
        let commitments = self.program_config.commitments.clone();
        let l1_chain_id = self.requester_config.l1_chain_id;
        let l2_chain_id = self.requester_config.l2_chain_id;

        let witness_gen_count = self
            .driver_config
            .driver_db_client
            .fetch_request_count(
                RequestStatus::WitnessGeneration,
                &commitments,
                l1_chain_id,
                l2_chain_id,
            )
            .await?;

        let execution_count = self
            .driver_config
            .driver_db_client
            .fetch_request_count(
                RequestStatus::Execution,
                &commitments,
                l1_chain_id,
                l2_chain_id,
            )
            .await?;

        let prove_count = self
            .driver_config
            .driver_db_client
            .fetch_request_count(RequestStatus::Prove, &commitments, l1_chain_id, l2_chain_id)
            .await?;

        // If there are already MAX_CONCURRENT_PROOF_REQUESTS proofs in WitnessGeneration, Execute, and Prove status, return.
        if witness_gen_count + execution_count + prove_count
            >= self.requester_config.max_concurrent_proof_requests as i64
        {
            debug!("There are already MAX_CONCURRENT_PROOF_REQUESTS proofs in WitnessGeneration, Execute, and Prove status.");
            return Ok(());
        }

        // If there are already MAX_CONCURRENT_WITNESS_GEN proofs in WitnessGeneration status, return.
        if witness_gen_count >= self.requester_config.max_concurrent_witness_gen as i64 {
            debug!(
                "There are already MAX_CONCURRENT_WITNESS_GEN proofs in WitnessGeneration status."
            );
            return Ok(());
        }

        if let Some(request) = self.get_next_unrequested_proof().await? {
            info!(
                request_id = request.id,
                request_type = ?request.req_type,
                start_block = request.start_block,
                end_block = request.end_block,
                "Making proof request"
            );
            let request_clone = request.clone();
            let proof_requester = self.proof_requester.clone();
            let handle =
                tokio::spawn(
                    async move { proof_requester.make_proof_request(request_clone).await },
                );
            let id = handle.id();
            self.tasks.lock().await.insert(id, (handle, request));
        }

        Ok(())
    }

    /// Get the next unrequested proof from the database.
    ///
    /// If there is an Aggregation proof with the same start block, range vkey commitment, and aggregation vkey, return that.
    /// Otherwise, return a range proof with the lowest start block.
    async fn get_next_unrequested_proof(&self) -> Result<Option<OPSuccinctRequest>> {
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        let unreq_agg_request = self
            .driver_config
            .driver_db_client
            .fetch_unrequested_agg_proof(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        if let Some(unreq_agg_request) = unreq_agg_request {
            return Ok(Some(unreq_agg_request));
        }

        let unreq_range_request = self
            .driver_config
            .driver_db_client
            .fetch_first_unrequested_range_proof(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        if let Some(unreq_range_request) = unreq_range_request {
            return Ok(Some(unreq_range_request));
        }

        Ok(None)
    }

    /// Relay all completed aggregation proofs to the contract.
    #[tracing::instrument(name = "proposer.submit_agg_proofs", skip(self))]
    async fn submit_agg_proofs(&self) -> Result<()> {
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        // See if there is an aggregation proof that is complete for this start block. NOTE: There should only be one "pending" aggregation proof at a time for a specific start block.
        let completed_agg_proof = self
            .driver_config
            .driver_db_client
            .fetch_completed_agg_proof_after_block(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        // If there are no completed aggregation proofs, do nothing.
        let completed_agg_proof = match completed_agg_proof {
            Some(proof) => proof,
            None => return Ok(()),
        };

        // Relay the aggregation proof.
        let transaction_hash = match self.relay_aggregation_proof(&completed_agg_proof).await {
            Ok(transaction_hash) => transaction_hash,
            Err(e) => {
                GaugeMetric::RelayAggProofErrorCount.increment(1.0);
                return Err(e);
            }
        };

        info!(
            "Relayed aggregation proof. Transaction hash: {:?}",
            transaction_hash
        );

        // Update the request to status RELAYED.
        self.driver_config
            .driver_db_client
            .update_request_to_relayed(
                completed_agg_proof.id,
                transaction_hash,
                self.contract_config.l2oo_address,
            )
            .await?;

        Ok(())
    }

    /// Submit the transaction to create a validity dispute game.
    ///
    /// If the DGF address is set, use it to create a new validity dispute game that will resolve with the proof. Otherwise, propose the L2 output.
    async fn relay_aggregation_proof(
        &self,
        completed_agg_proof: &OPSuccinctRequest,
    ) -> Result<B256> {
        // Get the output at the end block of the last completed aggregation proof.
        let output = self
            .driver_config
            .fetcher
            .get_l2_output_at_block(completed_agg_proof.end_block as u64)
            .await?;

        // If the DisputeGameFactory address is set, use it to create a new validity dispute game that will resolve with the proof.
        // Note: In the DGF setting, the proof immediately resolves the game.
        // Otherwise, propose the L2 output.
        let receipt = if self.contract_config.dgf_address != Address::ZERO {
            // Validity game type: https://github.com/ethereum-optimism/optimism/blob/develop/packages/contracts-bedrock/src/dispute/lib/Types.sol#L64.
            const OP_SUCCINCT_VALIDITY_DISPUTE_GAME_TYPE: u32 = 6;

            // Get the initialization bond for the validity dispute game.
            let init_bond = self
                .contract_config
                .dgf_contract
                .initBonds(OP_SUCCINCT_VALIDITY_DISPUTE_GAME_TYPE)
                .call()
                .await?
                ._0;

            let extra_data = ValidityDisputeGameExtraData {
                l2_block_number: completed_agg_proof.end_block as u64,
                l1_block_number: completed_agg_proof.checkpointed_l1_block_number.unwrap() as u64,
                proof: completed_agg_proof.proof.as_ref().unwrap().clone().into(),
            }
            .abi_encode();

            self.contract_config
                .dgf_contract
                .create(
                    OP_SUCCINCT_VALIDITY_DISPUTE_GAME_TYPE,
                    output.output_root,
                    extra_data.into(),
                )
                .value(init_bond)
                .send()
                .await?
                .with_required_confirmations(NUM_CONFIRMATIONS)
                .with_timeout(Some(Duration::from_secs(TIMEOUT)))
                .get_receipt()
                .await?
        } else {
            // Propose the L2 output.
            self.contract_config
                .l2oo_contract
                .proposeL2Output(
                    output.output_root,
                    U256::from(completed_agg_proof.end_block),
                    U256::from(completed_agg_proof.checkpointed_l1_block_number.unwrap()),
                    completed_agg_proof.proof.clone().unwrap().into(),
                )
                .send()
                .await?
                .with_required_confirmations(NUM_CONFIRMATIONS)
                .with_timeout(Some(Duration::from_secs(TIMEOUT)))
                .get_receipt()
                .await?
        };

        // If the transaction reverted, log the error.
        if !receipt.status() {
            return Err(anyhow!("Transaction reverted: {:?}", receipt));
        }

        Ok(receipt.transaction_hash())
    }

    /// Validate the requester config matches the contract.
    async fn validate_contract_config(&self) -> Result<()> {
        // Validate the requester config matches the contract.
        let contract_rollup_config_hash = self
            .contract_config
            .l2oo_contract
            .rollupConfigHash()
            .call()
            .await?
            .rollupConfigHash;
        let contract_agg_vkey_hash = self
            .contract_config
            .l2oo_contract
            .aggregationVkey()
            .call()
            .await?
            .aggregationVkey;
        let contract_range_vkey_commitment = self
            .contract_config
            .l2oo_contract
            .rangeVkeyCommitment()
            .call()
            .await?
            .rangeVkeyCommitment;

        let rollup_config_hash_match =
            contract_rollup_config_hash == self.program_config.commitments.rollup_config_hash;
        let agg_vkey_hash_match =
            contract_agg_vkey_hash == self.program_config.commitments.agg_vkey_hash;
        let range_vkey_commitment_match =
            contract_range_vkey_commitment == self.program_config.commitments.range_vkey_commitment;

        if !rollup_config_hash_match || !agg_vkey_hash_match || !range_vkey_commitment_match {
            tracing::error!(
                rollup_config_hash_match = rollup_config_hash_match,
                agg_vkey_hash_match = agg_vkey_hash_match,
                range_vkey_commitment_match = range_vkey_commitment_match,
                "Config mismatches detected."
            );

            if !rollup_config_hash_match {
                tracing::error!(
                    received = ?contract_rollup_config_hash,
                    expected = ?self.program_config.commitments.rollup_config_hash,
                    "Rollup config hash mismatch"
                );
            }

            if !agg_vkey_hash_match {
                tracing::error!(
                    received = ?contract_agg_vkey_hash,
                    expected = ?self.program_config.commitments.agg_vkey_hash,
                    "Aggregation vkey hash mismatch"
                );
            }

            if !range_vkey_commitment_match {
                tracing::error!(
                    received = ?contract_range_vkey_commitment,
                    expected = ?self.program_config.commitments.range_vkey_commitment,
                    "Range vkey commitment mismatch"
                );
            }

            return Err(anyhow::anyhow!("Config mismatches detected. Please run {{cargo run --bin config --release -- --env-file ENV_FILE}} to get the expected config for your contract."));
        }

        Ok(())
    }

    /// Spawns thread for handling task completion and error handling.
    async fn spawn_task_completion_handler(&self) -> Result<()> {
        let tasks = self.tasks.clone();
        let proof_requester = self.proof_requester.clone();

        tokio::spawn(async move {
            loop {
                let mut tasks = tasks.lock().await;
                let mut completed = Vec::new();

                // Check and process completed tasks
                for (id, (handle, _)) in tasks.iter() {
                    if handle.is_finished() {
                        completed.push(*id);
                    }
                }

                // Process completed tasks - this will properly await and drop them
                for id in completed {
                    if let Some((handle, request)) = tasks.remove(&id) {
                        // First await the handle to properly clean up the task.
                        match handle.await {
                            Ok(result) => {
                                if let Err(e) = result {
                                    error!(
                                        request_id = request.id,
                                        request_type = ?request.req_type,
                                        error = ?e,
                                        "Task failed with error"
                                    );
                                    // Now safe to retry as original task is cleaned up
                                    match proof_requester
                                        .retry_request(
                                            request,
                                            ExecutionStatus::UnspecifiedExecutionStatus,
                                        )
                                        .await
                                    {
                                        Ok(_) => {
                                            GaugeMetric::ProofRequestRetryCount.increment(1.0);
                                        }
                                        Err(retry_err) => {
                                            error!(error = ?retry_err, "Failed to retry request");
                                            GaugeMetric::RetryErrorCount.increment(1.0);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    request_id = request.id,
                                    request_type = ?request.req_type,
                                    error = ?e,
                                    "Task panicked"
                                );
                                // Now safe to retry as original task is cleaned up
                                match proof_requester
                                    .retry_request(
                                        request,
                                        ExecutionStatus::UnspecifiedExecutionStatus,
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        GaugeMetric::ProofRequestRetryCount.increment(1.0);
                                    }
                                    Err(retry_err) => {
                                        error!(error = ?retry_err, "Failed to retry request after panic");
                                        GaugeMetric::RetryErrorCount.increment(1.0);
                                    }
                                }
                            }
                        }
                    }
                }

                drop(tasks);
                tokio::time::sleep(Duration::from_secs(TASK_COMPLETION_HANDLER_INTERVAL)).await;
            }
        });

        Ok(())
    }

    /// Initialize the proposer by cleaning up stale requests and creating new range proof requests for the proposer with the given chain ID.
    ///
    /// This function performs several key tasks:
    /// 1. Validates that the proposer's config matches the contract
    /// 2. Deletes unrecoverable requests (UNREQUESTED, EXECUTION, WITNESS_GENERATION)
    /// 3. Cancels PROVE requests with mismatched commitment configs
    /// 4. Identifies gaps between the latest proposed block and finalized block
    /// 5. Creates new range proof requests to cover those gaps
    ///
    /// The goal is to ensure the database is in a clean state and all block ranges
    /// between the latest proposed block and finalized block have corresponding requests.
    #[tracing::instrument(name = "proposer.initialize_proposer", skip(self))]
    async fn initialize_proposer(&self) -> Result<()> {
        // Validate the requester config matches the contract.
        self.validate_contract_config()
            .await
            .context("Failed to validate the requester config matches the contract.")?;

        // Delete all requests for the same chain ID that are of status UNREQUESTED, EXECUTION or WITNESS_GENERATION as they're unrecoverable.
        self.driver_config
            .driver_db_client
            .delete_all_requests_with_statuses(
                &[
                    RequestStatus::Unrequested,
                    RequestStatus::Execution,
                    RequestStatus::WitnessGeneration,
                ],
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        info!("Deleted all unrecoverable requests.");

        // Cancel all requests in PROVE state for the same chain ID that have a different commitment config.
        self.driver_config
            .driver_db_client
            .cancel_prove_requests_with_different_commitment_config(
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        // Get the latest proposed block number on the contract.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        let finalized_block_number = self
            .driver_config
            .fetcher
            .get_l2_header(BlockId::finalized())
            .await?
            .number;

        // Get all requests in PROVE or COMPLETE status with the same commitment config and start block >= latest_proposed_block_number.
        // These requests are non-overlapping.
        let mut requests = self
            .driver_config
            .driver_db_client
            .fetch_ranges_after_block(
                &[RequestStatus::Prove, RequestStatus::Complete],
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        // Sort the requests by start block.
        requests.sort_by_key(|r| r.0);

        let disjoint_ranges = find_gaps(
            latest_proposed_block_number as i64,
            finalized_block_number as i64,
            &requests,
        );

        let ranges_to_prove = get_ranges_to_prove(
            &disjoint_ranges,
            self.requester_config.range_proof_interval as i64,
        );

        info!("Found {} ranges to prove.", ranges_to_prove.len());

        // Create range proof requests for the ranges to prove in parallel
        let new_range_requests = stream::iter(ranges_to_prove)
            .map(|range| {
                let mode = if self.requester_config.mock {
                    RequestMode::Mock
                } else {
                    RequestMode::Real
                };
                OPSuccinctRequest::create_range_request(
                    mode,
                    range.0,
                    range.1,
                    self.program_config.commitments.range_vkey_commitment,
                    self.program_config.commitments.rollup_config_hash,
                    self.requester_config.l1_chain_id,
                    self.requester_config.l2_chain_id,
                    self.driver_config.fetcher.clone(),
                )
            })
            .buffer_unordered(10) // Do 10 at a time, otherwise it's too slow when fetching the block range data.
            .try_collect::<Vec<OPSuccinctRequest>>()
            .await?;

        info!("Inserting new range proof requests into the database.");

        // Insert the new range proof requests into the database.
        self.driver_config
            .driver_db_client
            .insert_requests(&new_range_requests)
            .await?;

        Ok(())
    }

    /// Fetch and log the proposer metrics.
    async fn log_proposer_metrics(&self) -> Result<()> {
        // Get the latest proposed block number on the contract.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        // Get all completed range proofs from the database.
        let completed_range_proofs = self
            .driver_config
            .driver_db_client
            .fetch_completed_ranges(
                &self.program_config.commitments,
                latest_proposed_block_number as i64,
                self.requester_config.l1_chain_id,
                self.requester_config.l2_chain_id,
            )
            .await?;

        // Get the highest proven contiguous block.
        let highest_block_number = self
            .get_highest_proven_contiguous_block(completed_range_proofs)?
            .map_or(latest_proposed_block_number, |block| block as u64);

        // Fetch request counts for different statuses
        let commitments = &self.program_config.commitments;
        let l1_chain_id = self.requester_config.l1_chain_id;
        let l2_chain_id = self.requester_config.l2_chain_id;
        let db_client = &self.driver_config.driver_db_client;

        // Define statuses and their corresponding variable names
        let (
            num_unrequested_requests,
            num_prove_requests,
            num_execution_requests,
            num_witness_generation_requests,
        ) = (
            db_client
                .fetch_request_count(
                    RequestStatus::Unrequested,
                    commitments,
                    l1_chain_id,
                    l2_chain_id,
                )
                .await?,
            db_client
                .fetch_request_count(RequestStatus::Prove, commitments, l1_chain_id, l2_chain_id)
                .await?,
            db_client
                .fetch_request_count(
                    RequestStatus::Execution,
                    commitments,
                    l1_chain_id,
                    l2_chain_id,
                )
                .await?,
            db_client
                .fetch_request_count(
                    RequestStatus::WitnessGeneration,
                    commitments,
                    l1_chain_id,
                    l2_chain_id,
                )
                .await?,
        );

        // Log metrics
        info!(
            target: "proposer_metrics",
            "unrequested={num_unrequested_requests} prove={num_prove_requests} execution={num_execution_requests} witness_generation={num_witness_generation_requests} highest_contiguous_proven_block={highest_block_number}"
        );

        // Update gauges for proof counts
        GaugeMetric::CurrentUnrequestedProofs.set(num_unrequested_requests as f64);
        GaugeMetric::CurrentProvingProofs.set(num_prove_requests as f64);
        GaugeMetric::CurrentWitnessgenProofs.set(num_witness_generation_requests as f64);
        GaugeMetric::CurrentExecuteProofs.set(num_execution_requests as f64);
        GaugeMetric::HighestProvenContiguousBlock.set(highest_block_number as f64);
        GaugeMetric::LatestContractL2Block.set(latest_proposed_block_number as f64);

        // Get and set L2 block metrics
        let fetcher = &self.proof_requester.fetcher;
        GaugeMetric::L2UnsafeHeadBlock
            .set(fetcher.get_l2_header(BlockId::latest()).await?.number as f64);
        GaugeMetric::L2FinalizedBlock
            .set(fetcher.get_l2_header(BlockId::finalized()).await?.number as f64);

        // Get submission interval from contract and set gauge
        let contract_submission_interval: u64 = self
            .contract_config
            .l2oo_contract
            .submissionInterval()
            .call()
            .await?
            .submissionInterval
            .try_into()
            .unwrap();

        let submission_interval =
            contract_submission_interval.max(self.requester_config.submission_interval);
        GaugeMetric::MinBlockToProveToAgg
            .set((latest_proposed_block_number + submission_interval) as f64);

        Ok(())
    }

    #[tracing::instrument(name = "proposer.run", skip(self))]
    pub async fn run(&self) -> Result<()> {
        // Spawn the task completion handler.
        self.spawn_task_completion_handler().await?;

        // Handle the case where the proposer is being re-started and the proposer state needs to be updated.
        self.initialize_proposer().await?;

        // Initialize the metrics gauges.
        init_gauges();

        // Loop interval in seconds.
        loop {
            // Wrap the entire loop body in a match to handle errors
            match self.run_loop_iteration().await {
                Ok(_) => {
                    // Normal sleep between iterations
                    tokio::time::sleep(Duration::from_secs(
                        self.driver_config.loop_interval_seconds,
                    ))
                    .await;
                }
                Err(e) => {
                    // Log the error
                    tracing::error!("Error in proposer loop: {}", e);
                    // Update the error gauge
                    GaugeMetric::TotalErrorCount.increment(1.0);
                    // Pause for 10 seconds before restarting
                    tracing::info!("Pausing for 10 seconds before restarting the process");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }

    // Run a single loop of the validity proposer.
    async fn run_loop_iteration(&self) -> Result<()> {
        // Validate the requester config matches the contract.
        self.validate_contract_config().await?;

        // Log the proposer metrics.
        self.log_proposer_metrics().await?;

        // Add new range requests to the database.
        self.add_new_ranges().await?;

        // Get all proof statuses of all requests in the proving state.
        self.handle_proving_requests().await?;

        // Create aggregation proofs based on the completed range proofs. Checkpoints the block hash associated with the aggregation proof
        // in advance.
        self.create_aggregation_proofs().await?;

        // Request all unrequested proofs from the prover network.
        self.request_queued_proofs().await?;

        // Submit any aggregation proofs that are complete.
        self.submit_agg_proofs().await?;

        Ok(())
    }

    /// Get the highest block number at the end of the largest contiguous range of completed range proofs.
    /// Returns None if there are no completed range proofs.
    fn get_highest_proven_contiguous_block(
        &self,
        completed_range_proofs: Vec<(i64, i64)>,
    ) -> Result<Option<i64>> {
        if completed_range_proofs.is_empty() {
            return Ok(None);
        }

        let mut highest_block = completed_range_proofs[0].0;
        let mut current_end = completed_range_proofs[0].1;

        for proof in completed_range_proofs.iter().skip(1) {
            if proof.0 == current_end {
                current_end = proof.1;
                highest_block = current_end;
            } else {
                break;
            }
        }

        Ok(Some(highest_block))
    }
}
