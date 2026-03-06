use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use anyhow::{bail, Context, Result};
use op_succinct_host_utils::metrics::MetricsGauge;
use op_succinct_proof_utils::{cluster_agg_proof, cluster_range_proof, get_range_elf_embedded};
use sp1_sdk::{
    network::{proto::types::FulfillmentStatus, NetworkMode},
    Elf, NetworkProver, ProveRequest, Prover, SP1ProofMode, SP1ProofWithPublicValues,
    SP1ProvingKey, SP1Stdin, SP1VerifyingKey, SP1_CIRCUIT_VERSION,
};
use tokio::time::sleep;

use crate::{config::ProofProviderConfig, prometheus::ProposerGauge};

/// Polling interval (in seconds) for checking proof status.
/// Matches the SP1 SDK's internal polling interval:
/// https://github.com/succinctlabs/sp1/blob/dev/crates/sdk/src/network/prover.rs#L551
pub const PROOF_STATUS_POLL_INTERVAL: u64 = 2;

/// Unique identifier for a proof request.
pub type ProofId = B256;

/// Get the current Unix timestamp in seconds.
///
/// Panics if system time is before Unix epoch (should never happen).
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time before Unix epoch")
        .as_secs()
}

/// Container for proving and verifying keys.
#[derive(Clone)]
pub struct ProofKeys {
    pub range_pk: Arc<SP1ProvingKey>,
    pub range_vk: Arc<SP1VerifyingKey>,
    pub agg_pk: Arc<SP1ProvingKey>,
    pub agg_vk: Arc<SP1VerifyingKey>,
}

/// Proof provider abstraction for generating range and aggregation proofs.
///
/// This enum wraps the concrete provider implementations, allowing the proposer
/// to be agnostic about how proofs are generated (network vs mock).
#[derive(Clone)]
pub enum ProofProvider {
    /// Network-based proving via SP1 prover network.
    Network(NetworkProofProvider),
    /// Local mock execution (creates mock proofs, no real proving).
    Mock(MockProofProvider),
    /// Self-hosted cluster proving via sp1-cluster API.
    Cluster(ClusterProofProvider),
}

impl ProofProvider {
    /// Generate a range proof.
    ///
    /// In mock mode: executes locally and returns execution stats.
    /// In network mode: submits to network, waits for completion, returns (proof, 0, 0).
    ///
    /// Returns: (proof, instruction_cycles, sp1_gas)
    pub async fn generate_range_proof(
        &self,
        stdin: SP1Stdin,
    ) -> Result<(SP1ProofWithPublicValues, u64, u64)> {
        match self {
            ProofProvider::Network(p) => p.generate_range_proof(stdin).await,
            ProofProvider::Mock(p) => p.generate_range_proof(stdin).await,
            ProofProvider::Cluster(p) => p.generate_range_proof(stdin).await,
        }
    }

    /// Generate an aggregation proof.
    ///
    /// In mock mode: executes locally and creates mock proof.
    /// In network mode: submits to network, waits for completion.
    /// In cluster mode: submits to self-hosted cluster, waits for completion.
    pub async fn generate_agg_proof(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        match self {
            ProofProvider::Network(p) => p.generate_agg_proof(stdin).await,
            ProofProvider::Mock(p) => p.generate_agg_proof(stdin).await,
            ProofProvider::Cluster(p) => p.generate_agg_proof(stdin).await,
        }
    }

    /// Access to proving keys.
    pub fn keys(&self) -> &ProofKeys {
        match self {
            ProofProvider::Network(p) => &p.keys,
            ProofProvider::Mock(p) => &p.keys,
            ProofProvider::Cluster(p) => &p.keys,
        }
    }

    /// Access to configuration.
    pub fn config(&self) -> &ProofProviderConfig {
        match self {
            ProofProvider::Network(p) => &p.config,
            ProofProvider::Mock(p) => &p.config,
            ProofProvider::Cluster(p) => &p.config,
        }
    }
}

/// Network-based proof provider using SP1 prover network.
#[derive(Clone)]
pub struct NetworkProofProvider {
    prover: Arc<NetworkProver>,
    keys: ProofKeys,
    config: ProofProviderConfig,
    network_mode: NetworkMode,
}

impl NetworkProofProvider {
    pub fn new(
        prover: Arc<NetworkProver>,
        keys: ProofKeys,
        config: ProofProviderConfig,
        network_mode: NetworkMode,
    ) -> Self {
        Self { prover, keys, config, network_mode }
    }

    /// Get a reference to the underlying network prover.
    pub fn inner(&self) -> &NetworkProver {
        &self.prover
    }

    /// Generate a range proof via network.
    pub async fn generate_range_proof(
        &self,
        stdin: SP1Stdin,
    ) -> Result<(SP1ProofWithPublicValues, u64, u64)> {
        tracing::info!("Generating range proof via network");
        let proof_id = self.request_range_proof(stdin).await?;
        let proof = self.wait_for_proof(proof_id).await?;
        Ok((proof, 0, 0))
    }

    /// Generate an aggregation proof via network.
    pub async fn generate_agg_proof(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        tracing::info!("Generating aggregation proof via network");
        let proof_id = self.request_agg_proof(stdin).await?;
        self.wait_for_proof(proof_id).await
    }

    /// Submit a range proof request to the network.
    async fn request_range_proof(&self, stdin: SP1Stdin) -> Result<ProofId> {
        let proof_id = self
            .prover
            .prove(&self.keys.range_pk, stdin)
            .compressed()
            .skip_simulation(true)
            .strategy(self.config.range_proof_strategy)
            .timeout(Duration::from_secs(self.config.timeout))
            .min_auction_period(self.config.min_auction_period)
            .max_price_per_pgu(self.config.max_price_per_pgu)
            .cycle_limit(self.config.range_cycle_limit)
            .gas_limit(self.config.range_gas_limit)
            .whitelist(self.config.whitelist.clone())
            .request()
            .await?;

        tracing::info!(proof_id = %proof_id, "Range proof request submitted");
        Ok(proof_id)
    }

    /// Submit an aggregation proof request to the network.
    async fn request_agg_proof(&self, stdin: SP1Stdin) -> Result<ProofId> {
        let proof_id = self
            .prover
            .prove(&self.keys.agg_pk, stdin)
            .mode(self.config.agg_proof_mode)
            .strategy(self.config.agg_proof_strategy)
            .timeout(Duration::from_secs(self.config.timeout))
            .min_auction_period(self.config.min_auction_period)
            .max_price_per_pgu(self.config.max_price_per_pgu)
            .cycle_limit(self.config.agg_cycle_limit)
            .gas_limit(self.config.agg_gas_limit)
            .whitelist(self.config.whitelist.clone())
            .request()
            .await?;

        tracing::info!(proof_id = %proof_id, "Aggregation proof request submitted");
        Ok(proof_id)
    }

    /// Wait for a proof to be fulfilled by polling the network.
    ///
    /// Timeout behavior:
    /// - **Proving timeout** (`config.timeout`): Overall maximum wait time from start.
    /// - **Network call timeout** (`config.network_calls_timeout`): Per-call timeout; retries on
    ///   failure.
    /// - **Auction timeout** (`config.auction_timeout`): Cancels if no prover picks up the request.
    /// - **Server deadline** (`status.deadline()`): Server-side proving deadline.
    async fn wait_for_proof(&self, proof_id: ProofId) -> Result<SP1ProofWithPublicValues> {
        let start_time = std::time::Instant::now();
        let proving_timeout = Duration::from_secs(self.config.timeout);
        let is_mainnet = self.network_mode == NetworkMode::Mainnet;

        loop {
            // Proving timeout - ensures we don't wait forever if network calls keep failing.
            if let ProvingTimeout::Exceeded { elapsed_secs } =
                check_timeout(start_time.elapsed(), proving_timeout)
            {
                tracing::warn!(
                    proof_id = %proof_id,
                    elapsed_secs,
                    timeout_secs = self.config.timeout,
                    "proving timeout exceeded"
                );
                ProposerGauge::ProvingTimeoutError.increment(1.0);
                bail!(
                    "Proving timeout: proof_id={}, elapsed={}s, timeout={}s \
                    (consider reducing workload: increase RANGE_SPLIT_COUNT or reduce PROPOSAL_INTERVAL_IN_BLOCKS)",
                    proof_id,
                    elapsed_secs,
                    self.config.timeout
                );
            }

            // Get proof status - retry on transient failures.
            let (status, proof) = match self
                .network_call_with_timeout(
                    self.prover.get_proof_status(proof_id),
                    "get_proof_status",
                    proof_id,
                )
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(proof_id = %proof_id, error = %e, "get_proof_status failed, retrying...");
                    sleep(Duration::from_secs(PROOF_STATUS_POLL_INTERVAL)).await;
                    continue;
                }
            };

            // Get proof request details for auction timeout check - retry on transient failures.
            let request_details = match self
                .network_call_with_timeout(
                    self.prover.get_proof_request(proof_id),
                    "get_proof_request",
                    proof_id,
                )
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(proof_id = %proof_id, error = %e, "get_proof_request failed, retrying...");
                    sleep(Duration::from_secs(PROOF_STATUS_POLL_INTERVAL)).await;
                    continue;
                }
            };

            let current_time = current_timestamp();

            // Check auction timeout: if request is still in "Requested" state past the deadline.
            // Only cancel on mainnet where auction dynamics are meaningful.
            if let Some(details) = &request_details {
                let timeout_secs = self.config.auction_timeout;
                if let AuctionTimeout::Exceeded { elapsed_secs } = check_auction(
                    is_mainnet,
                    details.fulfillment_status,
                    details.created_at,
                    timeout_secs,
                    current_time,
                ) {
                    tracing::warn!(
                        proof_id = %proof_id,
                        elapsed_secs,
                        timeout_secs,
                        "auction timeout exceeded, cancelling request"
                    );
                    if let Err(e) = self
                        .network_call_with_timeout(
                            self.prover.cancel_request(proof_id),
                            "cancel_request",
                            proof_id,
                        )
                        .await
                    {
                        tracing::error!(proof_id = %proof_id, error = %e, "Failed to cancel proof request");
                    }
                    ProposerGauge::AuctionTimeoutError.increment(1.0);
                    bail!(
                        "Auction timeout: proof_id={}, elapsed={}s, timeout={}s",
                        proof_id,
                        elapsed_secs,
                        timeout_secs
                    );
                }
            }

            // Check if the proof deadline has passed.
            if let Deadline::Exceeded { deadline } = check_deadline(status.deadline(), current_time)
            {
                tracing::warn!(
                    proof_id = %proof_id,
                    deadline,
                    current_time,
                    "Proof request deadline exceeded"
                );
                ProposerGauge::DeadlineExceededError.increment(1.0);
                bail!(
                    "Deadline exceeded: proof_id={}, deadline={}, current_time={} \
                     (consider reducing workload: increase RANGE_SPLIT_COUNT or reduce PROPOSAL_INTERVAL_IN_BLOCKS)",
                    proof_id,
                    deadline,
                    current_time
                );
            }

            // Check fulfillment status.
            match check_status(status.fulfillment_status()) {
                ProofStatus::Ready => {
                    tracing::info!(proof_id = %proof_id, "Proof fulfilled");
                    return proof.ok_or_else(|| {
                        anyhow::anyhow!("Proof status is fulfilled but proof is None")
                    });
                }
                ProofStatus::Failed => {
                    bail!(
                        "Proving failed: proof_id={}, execution_status={}",
                        proof_id,
                        status.execution_status()
                    );
                }
                ProofStatus::Pending => {
                    tracing::debug!(proof_id = %proof_id, "Proof pending/assigned, continuing...");
                }
            }

            sleep(Duration::from_secs(PROOF_STATUS_POLL_INTERVAL)).await;
        }
    }

    /// Execute a network call with timeout.
    async fn network_call_with_timeout<F, T>(
        &self,
        future: F,
        operation: &str,
        proof_id: ProofId,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T, anyhow::Error>>,
    {
        let timeout_secs = self.config.network_calls_timeout;
        match tokio::time::timeout(Duration::from_secs(timeout_secs), future).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                tracing::warn!(proof_id = %proof_id, operation, error = %e, "Network error");
                Err(e)
            }
            Err(_) => {
                tracing::warn!(proof_id = %proof_id, operation, timeout_secs, "Network call timed out");
                ProposerGauge::NetworkCallTimeout.increment(1.0);
                bail!(
                    "Network timeout after {}s for {} (proof_id={})",
                    timeout_secs,
                    operation,
                    proof_id
                )
            }
        }
    }
}

/// Mock proof provider for local execution without network.
#[derive(Clone)]
pub struct MockProofProvider {
    prover: Arc<NetworkProver>,
    keys: ProofKeys,
    config: ProofProviderConfig,
    agg_elf: &'static [u8],
}

impl MockProofProvider {
    pub fn new(
        prover: Arc<NetworkProver>,
        keys: ProofKeys,
        config: ProofProviderConfig,
        agg_elf: &'static [u8],
    ) -> Self {
        Self { prover, keys, config, agg_elf }
    }

    /// Generate a range proof in mock mode.
    pub async fn generate_range_proof(
        &self,
        stdin: SP1Stdin,
    ) -> Result<(SP1ProofWithPublicValues, u64, u64)> {
        tracing::info!("Generating range proof in mock mode");

        let (public_values, report) = self
            .prover
            .execute(Elf::Static(get_range_elf_embedded()), stdin)
            .calculate_gas(true)
            .deferred_proof_verification(false)
            .await
            .context("Mock range proof execution failed")?;

        let total_instruction_cycles = report.total_instruction_count();
        let total_sp1_gas = report.gas().unwrap_or(0);

        ProposerGauge::TotalInstructionCycles.set(total_instruction_cycles as f64);
        ProposerGauge::TotalSP1Gas.set(total_sp1_gas as f64);

        tracing::info!(
            total_instruction_cycles = total_instruction_cycles,
            total_sp1_gas = total_sp1_gas,
            "Captured execution stats for range proof"
        );

        let proof = SP1ProofWithPublicValues::create_mock_proof(
            &self.keys.range_vk,
            public_values,
            SP1ProofMode::Compressed,
            SP1_CIRCUIT_VERSION,
        );

        Ok((proof, total_instruction_cycles, total_sp1_gas))
    }

    /// Generate an aggregation proof in mock mode.
    pub async fn generate_agg_proof(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        tracing::info!("Generating aggregation proof in mock mode");

        let (public_values, _) = self
            .prover
            .execute(Elf::Static(self.agg_elf), stdin)
            .deferred_proof_verification(false)
            .await
            .context("Mock aggregation proof execution failed")?;

        Ok(SP1ProofWithPublicValues::create_mock_proof(
            &self.keys.agg_vk,
            public_values,
            self.config.agg_proof_mode,
            SP1_CIRCUIT_VERSION,
        ))
    }
}

/// Cluster-based proof provider using a self-hosted sp1-cluster.
#[derive(Clone)]
pub struct ClusterProofProvider {
    keys: ProofKeys,
    config: ProofProviderConfig,
}

impl ClusterProofProvider {
    pub fn new(keys: ProofKeys, config: ProofProviderConfig) -> Self {
        Self { keys, config }
    }

    pub async fn generate_range_proof(
        &self,
        stdin: SP1Stdin,
    ) -> Result<(SP1ProofWithPublicValues, u64, u64)> {
        let proof = cluster_range_proof(self.config.timeout, stdin).await?;
        // Cluster API does not report execution cycle or gas metrics.
        Ok((proof, 0, 0))
    }

    pub async fn generate_agg_proof(&self, stdin: SP1Stdin) -> Result<SP1ProofWithPublicValues> {
        cluster_agg_proof(self.config.timeout, self.config.agg_proof_mode, stdin).await
    }
}

/// Result of checking if proving has timed out.
#[derive(Debug, PartialEq)]
pub enum ProvingTimeout {
    /// Still within timeout, continue polling.
    Ok,
    /// Timeout exceeded, should bail.
    Exceeded { elapsed_secs: u64 },
}

/// Check if the overall proving timeout has been exceeded.
pub fn check_timeout(elapsed: Duration, timeout: Duration) -> ProvingTimeout {
    if elapsed > timeout {
        ProvingTimeout::Exceeded { elapsed_secs: elapsed.as_secs() }
    } else {
        ProvingTimeout::Ok
    }
}

/// Result of checking auction timeout.
#[derive(Debug, PartialEq)]
pub enum AuctionTimeout {
    /// No timeout issue, continue.
    Ok,
    /// Not applicable (not mainnet or already assigned).
    Skip,
    /// Auction timed out, should cancel and bail.
    Exceeded { elapsed_secs: u64 },
}

/// Check if the auction has timed out (no prover picked up the request).
///
/// Only applies on mainnet when the request is still in "Requested" state.
pub fn check_auction(
    is_mainnet: bool,
    fulfillment_status: i32,
    created_at: u64,
    auction_timeout: u64,
    current_time: u64,
) -> AuctionTimeout {
    if !is_mainnet {
        return AuctionTimeout::Skip;
    }

    if fulfillment_status != FulfillmentStatus::Requested as i32 {
        return AuctionTimeout::Skip;
    }

    let deadline = created_at + auction_timeout;
    if current_time > deadline {
        AuctionTimeout::Exceeded { elapsed_secs: current_time - created_at }
    } else {
        AuctionTimeout::Ok
    }
}

/// Result of checking server deadline.
#[derive(Debug, PartialEq)]
pub enum Deadline {
    /// Still within deadline.
    Ok,
    /// Deadline exceeded.
    Exceeded { deadline: u64 },
}

/// Check if the server-side proof deadline has been exceeded.
pub fn check_deadline(deadline: u64, current_time: u64) -> Deadline {
    if current_time >= deadline {
        Deadline::Exceeded { deadline }
    } else {
        Deadline::Ok
    }
}

/// Result of checking proof fulfillment status.
#[derive(Debug, PartialEq)]
pub enum ProofStatus {
    /// Proof is ready, return it.
    Ready,
    /// Proof request failed permanently.
    Failed,
    /// Proof is being worked on, continue polling.
    Pending,
}

/// Determine proof status from fulfillment status.
pub fn check_status(status: i32) -> ProofStatus {
    match FulfillmentStatus::try_from(status) {
        Ok(FulfillmentStatus::Fulfilled) => ProofStatus::Ready,
        Ok(FulfillmentStatus::Unfulfillable) => ProofStatus::Failed,
        _ => ProofStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::time::Duration;

    #[rstest]
    #[case::ok(30, 60, ProvingTimeout::Ok)]
    #[case::at_limit(60, 60, ProvingTimeout::Ok)]
    #[case::exceeded(61, 60, ProvingTimeout::Exceeded { elapsed_secs: 61 })]
    fn test_timeout(#[case] elapsed: u64, #[case] limit: u64, #[case] expected: ProvingTimeout) {
        let result = check_timeout(Duration::from_secs(elapsed), Duration::from_secs(limit));
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::not_mainnet(false, FulfillmentStatus::Requested as i32, 2000, AuctionTimeout::Skip)]
    #[case::assigned(true, FulfillmentStatus::Assigned as i32, 2000, AuctionTimeout::Skip)]
    #[case::fulfilled(true, FulfillmentStatus::Fulfilled as i32, 2000, AuctionTimeout::Skip)]
    #[case::within(true, FulfillmentStatus::Requested as i32, 1050, AuctionTimeout::Ok)]
    #[case::at_limit(true, FulfillmentStatus::Requested as i32, 1060, AuctionTimeout::Ok)]
    #[case::exceeded(true, FulfillmentStatus::Requested as i32, 1061, AuctionTimeout::Exceeded { elapsed_secs: 61 })]
    fn test_auction(
        #[case] is_mainnet: bool,
        #[case] status: i32,
        #[case] current_time: u64,
        #[case] expected: AuctionTimeout,
    ) {
        let result = check_auction(is_mainnet, status, 1000, 60, current_time);
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::ok(2000, 1500, Deadline::Ok)]
    #[case::at_limit(2000, 2000, Deadline::Exceeded { deadline: 2000 })]
    #[case::exceeded(2000, 2001, Deadline::Exceeded { deadline: 2000 })]
    fn test_deadline(#[case] deadline: u64, #[case] current: u64, #[case] expected: Deadline) {
        assert_eq!(check_deadline(deadline, current), expected);
    }

    #[rstest]
    #[case::ready(FulfillmentStatus::Fulfilled as i32, ProofStatus::Ready)]
    #[case::failed(FulfillmentStatus::Unfulfillable as i32, ProofStatus::Failed)]
    #[case::pending_assigned(FulfillmentStatus::Assigned as i32, ProofStatus::Pending)]
    #[case::pending_requested(FulfillmentStatus::Requested as i32, ProofStatus::Pending)]
    #[case::pending_unknown(999, ProofStatus::Pending)]
    fn test_status(#[case] status: i32, #[case] expected: ProofStatus) {
        assert_eq!(check_status(status), expected);
    }
}
