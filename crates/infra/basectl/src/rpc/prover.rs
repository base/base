//! Prover-service requester client helpers for the `basectl proofs` command group.

use std::{fmt, time::Duration};

use alloy_primitives::{Address, B256};
use base_prover_service_client::{
    ProofRequesterClient, ProverServiceClientBuildError, ProverServiceClientConfig,
    ProverServiceClientError,
};
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofRequest,
    ProofRequestKind, ProofSessionId, ProofStatus, ProveBlockRangeRequest, SnarkPlonkProofRequest,
    ZkBackend, ZkProofRequest, ZkVm,
};
use jsonrpsee::core::client::Error as JsonRpcClientError;
use tokio::time::{Instant, sleep};
use tracing::{debug, info};
use url::Url;

use crate::{
    errors::ProofsCommandError,
    rpc::games::{GameDetails, GameStatus},
};

/// Parameters for a `basectl proofs propose` game-matched PLONK proof request.
///
/// The `AggregateVerifier` contract reconstructs the proof journal from its
/// own stored game state with the submitting wallet as proposer, so every
/// range parameter must be taken from the target game itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofProposeRequest {
    /// Dispute game proxy address the proof targets.
    pub game: Address,
    /// Pre-state L2 block number (start of the proved range).
    pub pre_state_block: u64,
    /// Number of consecutive L2 blocks to prove.
    pub num_blocks: u64,
    /// L1 head hash stored at game creation time.
    pub l1_head: B256,
    /// Intermediate output root interval matching the game's checkpoints.
    pub intermediate_root_interval: u64,
    /// L1 wallet address that will later submit the proof on chain.
    ///
    /// The proof journal commits to this address as the proposer, so the
    /// `verifyProposalProof` transaction must be sent from exactly this
    /// wallet.
    pub prover_address: Address,
    /// ZK proving backend that executes the proof.
    pub zk_backend: ZkBackend,
    /// Composite ZK artifact hash required by the target game.
    pub zk_artifact_hash: B256,
    /// Explicit session ID override. When `None`, an idempotent session ID is
    /// derived from the network name, game, block range, checkpoint stride,
    /// and prover address.
    pub session_id: Option<String>,
}

impl ProofProposeRequest {
    /// Session ID namespace for proofs requested via basectl.
    ///
    /// v2 prevents reuse of prover-service or SP1 journals created before schedule pinning.
    const SESSION_NAMESPACE: &'static [u8] = b"basectl/v2";

    /// Builds a game-matched request from the game's on-chain state.
    ///
    /// Validates that the game can still accept a ZK proposal proof and takes
    /// the block range, L1 head, and checkpoint stride from the game so the
    /// proof journal matches what the contract reconstructs.
    /// `intermediate_root_interval` supplies the stride when the game does
    /// not expose `INTERMEDIATE_BLOCK_INTERVAL`; otherwise it must match the
    /// game's committed value.
    pub fn for_game(
        details: &GameDetails,
        prover_address: Address,
        zk_backend: ZkBackend,
        zk_artifact_hash: B256,
        session_id: Option<String>,
        intermediate_root_interval: Option<u64>,
    ) -> Result<Self, ProofsCommandError> {
        let not_provable = |reason: &str| ProofsCommandError::GameNotProvable {
            game: details.address.to_string(),
            reason: reason.to_string(),
        };
        if prover_address == Address::ZERO {
            return Err(not_provable("prover address cannot be the zero address"));
        }
        if details.status != GameStatus::InProgress {
            return Err(not_provable("game is not in progress"));
        }
        if details.zk_prover != Address::ZERO {
            return Err(not_provable("game already has a ZK proof"));
        }
        let intermediate_root_interval =
            Self::intermediate_root_interval(details, intermediate_root_interval)?;
        Ok(Self {
            game: details.address,
            pre_state_block: details.starting_block,
            num_blocks: details.block_interval,
            l1_head: details.l1_head,
            intermediate_root_interval,
            prover_address,
            zk_backend,
            zk_artifact_hash,
            session_id,
        })
    }

    /// Returns the explicit or derived session ID for an existing game's proof.
    ///
    /// Unlike [`Self::for_game`], this intentionally ignores mutable game
    /// status and proof slots so `basectl proofs submit` can retrieve a paid
    /// proof before its final on-chain submission preflight.
    pub fn session_id_for_game(
        network: &str,
        details: &GameDetails,
        prover_address: Address,
        zk_backend: ZkBackend,
        zk_artifact_hash: B256,
        session_id: Option<String>,
        intermediate_root_interval: Option<u64>,
    ) -> Result<String, ProofsCommandError> {
        if let Some(session_id) = session_id {
            return Ok(session_id);
        }
        let intermediate_root_interval =
            Self::intermediate_root_interval(details, intermediate_root_interval)?;
        Ok(Self {
            game: details.address,
            pre_state_block: details.starting_block,
            num_blocks: details.block_interval,
            l1_head: details.l1_head,
            intermediate_root_interval,
            prover_address,
            zk_backend,
            zk_artifact_hash,
            session_id: None,
        }
        .derive_session_id(network))
    }

    fn intermediate_root_interval(
        details: &GameDetails,
        intermediate_root_interval: Option<u64>,
    ) -> Result<u64, ProofsCommandError> {
        let not_provable = |reason: &str| ProofsCommandError::GameNotProvable {
            game: details.address.to_string(),
            reason: reason.to_string(),
        };
        if details.block_interval == 0 {
            return Err(not_provable("game covers an empty block range"));
        }
        if let (Some(explicit), Some(canonical)) =
            (intermediate_root_interval, details.intermediate_root_interval)
            && explicit != canonical
        {
            return Err(not_provable(&format!(
                "intermediate root interval {explicit} does not match the game \
                 implementation's INTERMEDIATE_BLOCK_INTERVAL {canonical}; a proof with a \
                 different stride would not verify on chain"
            )));
        }
        let intermediate_root_interval =
            intermediate_root_interval.or(details.intermediate_root_interval).ok_or_else(|| {
                not_provable(
                    "the game does not expose INTERMEDIATE_BLOCK_INTERVAL; \
                     pass --intermediate-root-interval",
                )
            })?;
        if intermediate_root_interval == 0
            || !details.block_interval.is_multiple_of(intermediate_root_interval)
        {
            return Err(not_provable(&format!(
                "intermediate root interval {intermediate_root_interval} must be a nonzero \
                 divisor of the game's {}-block range",
                details.block_interval
            )));
        }

        let expected_root_count = details.block_interval / intermediate_root_interval;
        if expected_root_count != details.intermediate_root_count as u64 {
            return Err(not_provable(&format!(
                "game committed {} intermediate root(s) but its {}-block range at a \
                 {intermediate_root_interval}-block checkpoint interval covers \
                 {expected_root_count}; the game was not created with the canonical \
                 checkpoints and a proof would not verify on chain",
                details.intermediate_root_count, details.block_interval
            )));
        }
        Ok(intermediate_root_interval)
    }

    /// Returns the effective session ID for `network`.
    ///
    /// Uses the explicit override when set; otherwise derives an idempotent
    /// `UUIDv5` from the network name, game address, block range, checkpoint
    /// stride, and prover address so re-running the same command resolves to
    /// the same prover-service session instead of enqueueing a duplicate
    /// proof. `basectl proofs propose` and `basectl proofs submit` share this
    /// derivation so `submit` can find the session that `propose` created
    /// without the operator copying session IDs around.
    pub fn effective_session_id(&self, network: &str) -> String {
        self.session_id.clone().unwrap_or_else(|| self.derive_session_id(network))
    }

    fn derive_session_id(&self, network: &str) -> String {
        let subtype = match self.zk_backend {
            ZkBackend::DryRun => "zk/sp1/snark_plonk/dry_run",
            ZkBackend::Cluster => "zk/sp1/snark_plonk",
            ZkBackend::Network => "zk/sp1/snark_plonk/network",
        };
        ProofSessionId::derive_from_components(
            Self::SESSION_NAMESPACE,
            subtype,
            &[
                network.as_bytes(),
                self.game.as_slice(),
                &self.pre_state_block.to_be_bytes(),
                &self.num_blocks.to_be_bytes(),
                &self.intermediate_root_interval.to_be_bytes(),
                self.prover_address.as_slice(),
                self.zk_artifact_hash.as_slice(),
            ],
        )
    }

    /// Builds the prover-service prove-block-range request for `network`, deriving
    /// its effective session ID with [`Self::effective_session_id`].
    pub fn to_prove_request(&self, network: &str, retry_failed: bool) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: self.effective_session_id(network),
                request: ProofRequestKind::SnarkPlonk(SnarkPlonkProofRequest {
                    proof: ZkProofRequest {
                        start_block_number: self.pre_state_block,
                        number_of_blocks_to_prove: self.num_blocks,
                        sequence_window: None,
                        l1_head: Some(self.l1_head),
                        intermediate_root_interval: Some(self.intermediate_root_interval),
                        schedule_l2_block_number: None,
                        zk_artifact_hash: Some(self.zk_artifact_hash),
                        zk_vm: ZkVm::Sp1,
                        zk_backend: self.zk_backend,
                    },
                    prover_address: self.prover_address,
                }),
            },
            retry_failed,
        }
    }
}

/// Prover-service requester client used by the `basectl proofs` commands.
#[derive(Clone)]
pub struct ProofsClient {
    endpoint: String,
    requester: ProofRequesterClient,
    poll_interval: Duration,
    max_wait: Duration,
}

impl fmt::Debug for ProofsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofsClient")
            .field("endpoint", &self.endpoint)
            .field("poll_interval", &self.poll_interval)
            .field("max_wait", &self.max_wait)
            .finish_non_exhaustive()
    }
}

impl ProofsClient {
    /// Connects a requester client to the prover-service `endpoint`.
    pub fn connect(endpoint: &Url) -> Result<Self, ProofsCommandError> {
        let config = ProverServiceClientConfig::new(endpoint.as_str());
        let requester = ProofRequesterClient::connect(&config).map_err(|source| {
            let source = match source {
                ProverServiceClientBuildError::RpcTransport(error) => {
                    ProverServiceClientBuildError::RpcTransport(Self::sanitize_rpc_error(error))
                }
                other => other,
            };
            ProofsCommandError::BuildClient {
                endpoint: endpoint.origin().ascii_serialization(),
                source,
            }
        })?;
        Ok(Self {
            endpoint: endpoint.origin().ascii_serialization(),
            requester,
            poll_interval: config.poll_interval(),
            max_wait: config.max_wait(),
        })
    }

    /// Overrides the maximum time spent waiting for proof completion.
    #[must_use]
    pub const fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// Overrides the poll cadence used by [`Self::wait_for_completion`].
    #[cfg(test)]
    #[must_use]
    pub const fn with_wait_config(mut self, poll_interval: Duration, max_wait: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.max_wait = max_wait;
        self
    }

    /// Returns the CLI label for a proof status.
    pub const fn status_label(status: ProofStatus) -> &'static str {
        match status {
            ProofStatus::Queued => "queued",
            ProofStatus::Running => "running",
            ProofStatus::Succeeded => "succeeded",
            ProofStatus::Failed => "failed",
        }
    }

    /// Submits a prove-block-range request and returns the accepted session ID.
    pub async fn submit(
        &self,
        request: ProveBlockRangeRequest,
    ) -> Result<String, ProofsCommandError> {
        debug!(
            endpoint = %self.endpoint,
            session_id = %request.proof.session_id,
            "submitting prove-block-range request"
        );
        let response = self
            .requester
            .prove_block_range(request)
            .await
            .map_err(|error| self.rpc_error("prover_proveBlockRange", error))?;
        info!(
            endpoint = %self.endpoint,
            session_id = %response.session_id,
            "prove-block-range request accepted"
        );
        Ok(response.session_id)
    }

    /// Returns proof status and result data for `session_id`.
    pub async fn proof_status(
        &self,
        session_id: &str,
    ) -> Result<GetProofResponse, ProofsCommandError> {
        self.requester
            .get_proof(GetProofRequest { session_id: session_id.to_string() })
            .await
            .map_err(|error| self.rpc_error("prover_getProof", error))
    }

    /// Lists submitted proof requests.
    pub async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> Result<ListProofsResponse, ProofsCommandError> {
        self.requester
            .list_proofs(request)
            .await
            .map_err(|error| self.rpc_error("prover_listProofs", error))
    }

    /// Polls `session_id` until it reaches a terminal status or the wait
    /// window elapses.
    pub async fn wait_for_completion(
        &self,
        session_id: &str,
    ) -> Result<GetProofResponse, ProofsCommandError> {
        let started = Instant::now();
        loop {
            let response = self.proof_status(session_id).await?;
            if matches!(response.status, ProofStatus::Succeeded | ProofStatus::Failed) {
                return Ok(response);
            }

            let waited = started.elapsed();
            debug!(
                endpoint = %self.endpoint,
                session_id = %session_id,
                status = Self::status_label(response.status),
                waited_secs = waited.as_secs(),
                "proof not complete; polling again"
            );
            if waited >= self.max_wait {
                return Err(ProofsCommandError::WaitTimeout {
                    session_id: session_id.to_string(),
                    waited,
                    last_status: Self::status_label(response.status).to_string(),
                });
            }
            sleep(self.poll_interval.min(self.max_wait - waited)).await;
        }
    }

    fn rpc_error(
        &self,
        method: &'static str,
        source: ProverServiceClientError,
    ) -> ProofsCommandError {
        let source = match source {
            ProverServiceClientError::RpcTransport(error) => {
                ProverServiceClientError::RpcTransport(Self::sanitize_rpc_error(error))
            }
            other => other,
        };
        ProofsCommandError::Rpc { endpoint: self.endpoint.clone(), method, source }
    }

    fn sanitize_rpc_error(error: JsonRpcClientError) -> JsonRpcClientError {
        match error {
            JsonRpcClientError::Transport(_) | JsonRpcClientError::RestartNeeded(_) => {
                JsonRpcClientError::Custom("transport request failed".to_string())
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        error::Error as _,
        io,
        net::SocketAddr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::{Address, B256};
    use base_prover_service_protocol::{
        DeleteProofRequest, DeleteProofsByTeeSignerRequest, GetProofRequest, GetProofResponse,
        ListProofsRequest, ListProofsResponse, ProofRequestKind, ProofStatus,
        ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer, ZkBackend, ZkVm,
    };
    use jsonrpsee::{
        core::{RpcResult, async_trait, client::Error as JsonRpcClientError},
        server::{Server, ServerHandle},
        types::{ErrorObjectOwned, error::ErrorCode},
    };
    use url::Url;

    use super::{ProofProposeRequest, ProofsClient};
    use crate::{
        errors::ProofsCommandError,
        rpc::games::{GameDetails, GameStatus},
    };

    fn provable_game() -> GameDetails {
        GameDetails {
            address: Address::repeat_byte(0xAA),
            status: GameStatus::InProgress,
            root_claim: B256::repeat_byte(0x11),
            starting_block: 4000,
            target_block: 5000,
            block_interval: 1000,
            intermediate_root_interval: Some(100),
            intermediate_root_count: 10,
            l1_head: B256::repeat_byte(0x22),
            parent_address: Address::repeat_byte(0xBB),
            tee_prover: Address::repeat_byte(0xCC),
            zk_prover: Address::ZERO,
            proof_count: 1,
            created_at: 1_700_000_000,
            expected_resolution: 1_700_432_000,
            countered_index: None,
        }
    }

    fn propose_request() -> ProofProposeRequest {
        ProofProposeRequest::for_game(
            &provable_game(),
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            None,
        )
        .expect("provable game should build a request")
    }

    #[test]
    fn propose_for_game_maps_game_state() {
        let request = propose_request();

        assert_eq!(request.game, Address::repeat_byte(0xAA));
        assert_eq!(request.pre_state_block, 4000);
        assert_eq!(request.num_blocks, 1000);
        assert_eq!(request.l1_head, B256::repeat_byte(0x22));
        assert_eq!(request.intermediate_root_interval, 100);
        assert_eq!(request.prover_address, Address::repeat_byte(0xDD));
    }

    #[test]
    fn propose_for_game_rejects_unprovable_games() {
        let resolved = GameDetails { status: GameStatus::DefenderWins, ..provable_game() };
        let already_proven =
            GameDetails { zk_prover: Address::repeat_byte(0xEE), ..provable_game() };
        let empty_range = GameDetails { block_interval: 0, target_block: 4000, ..provable_game() };
        let no_stride = GameDetails { intermediate_root_interval: None, ..provable_game() };

        for details in [resolved, already_proven, empty_range, no_stride] {
            let error = ProofProposeRequest::for_game(
                &details,
                Address::repeat_byte(0xDD),
                ZkBackend::Network,
                B256::repeat_byte(0x33),
                None,
                None,
            )
            .expect_err("unprovable game should be rejected");
            assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
        }
    }

    #[test]
    fn propose_for_game_rejects_zero_prover_address() {
        let error = ProofProposeRequest::for_game(
            &provable_game(),
            Address::ZERO,
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            None,
        )
        .expect_err("zero address cannot submit the resulting proof");

        assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
    }

    #[test]
    fn propose_for_game_rejects_stride_overrides_conflicting_with_canonical_stride() {
        let error = ProofProposeRequest::for_game(
            &provable_game(),
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            Some(250),
        )
        .expect_err("stride that conflicts with the canonical interval must be rejected");
        assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
    }

    #[test]
    fn propose_for_game_rejects_invalid_stride_overrides() {
        let no_stride = GameDetails { intermediate_root_interval: None, ..provable_game() };
        for interval in [0, 300] {
            let error = ProofProposeRequest::for_game(
                &no_stride,
                Address::repeat_byte(0xDD),
                ZkBackend::Network,
                B256::repeat_byte(0x33),
                None,
                Some(interval),
            )
            .expect_err("stride that is zero or does not divide the range must be rejected");
            assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
        }
    }

    #[test]
    fn propose_for_game_accepts_stride_override() {
        let no_stride = GameDetails {
            intermediate_root_interval: None,
            intermediate_root_count: 4,
            ..provable_game()
        };
        let request = ProofProposeRequest::for_game(
            &no_stride,
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            Some(250),
        )
        .expect("stride override should build a request");

        assert_eq!(request.intermediate_root_interval, 250);
    }

    #[test]
    fn propose_for_game_rejects_noncanonical_root_count() {
        // 1000-block game at the canonical 100-block stride commits 10 roots;
        // a game holding 20 was not created with the canonical checkpoints.
        let extra_roots = GameDetails { intermediate_root_count: 20, ..provable_game() };
        let error = ProofProposeRequest::for_game(
            &extra_roots,
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            None,
        )
        .expect_err("root count contradicting the canonical stride must be rejected");
        assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
    }

    #[test]
    fn propose_for_game_rejects_override_inconsistent_with_root_count() {
        // Without a canonical stride the override is trusted, but it must still
        // cover every committed root: 500-block strides over 1000 blocks cover 2,
        // not the 10 the game committed.
        let no_stride = GameDetails { intermediate_root_interval: None, ..provable_game() };
        let error = ProofProposeRequest::for_game(
            &no_stride,
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            Some(500),
        )
        .expect_err("override that does not cover the committed roots must be rejected");
        assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
    }

    #[test]
    fn propose_session_id_is_distinct_per_game_prover_and_backend() {
        let request = propose_request();

        assert_eq!(
            request.effective_session_id("mainnet"),
            request.effective_session_id("mainnet")
        );

        let other_game =
            ProofProposeRequest { game: Address::repeat_byte(0x99), ..request.clone() };
        let other_prover =
            ProofProposeRequest { prover_address: Address::repeat_byte(0x88), ..request.clone() };
        let other_backend =
            ProofProposeRequest { zk_backend: ZkBackend::Cluster, ..request.clone() };
        let other_stride =
            ProofProposeRequest { intermediate_root_interval: 200, ..request.clone() };
        let other_artifact =
            ProofProposeRequest { zk_artifact_hash: B256::repeat_byte(0x44), ..request.clone() };

        let base_id = request.effective_session_id("mainnet");
        assert_ne!(base_id, other_game.effective_session_id("mainnet"));
        assert_ne!(base_id, other_prover.effective_session_id("mainnet"));
        assert_ne!(base_id, other_backend.effective_session_id("mainnet"));
        assert_ne!(base_id, other_stride.effective_session_id("mainnet"));
        assert_ne!(base_id, other_artifact.effective_session_id("mainnet"));
        assert_ne!(base_id, request.effective_session_id("sepolia"));
    }

    #[test]
    fn propose_explicit_session_id_overrides_derivation() {
        let request = ProofProposeRequest {
            session_id: Some("custom-propose".to_string()),
            ..propose_request()
        };

        assert_eq!(request.effective_session_id("mainnet"), "custom-propose");
    }

    #[test]
    fn submit_session_id_ignores_mutable_game_state() {
        let expected = propose_request().effective_session_id("mainnet");
        let unavailable = GameDetails {
            status: GameStatus::DefenderWins,
            zk_prover: Address::repeat_byte(0xEE),
            ..provable_game()
        };

        let session_id = ProofProposeRequest::session_id_for_game(
            "mainnet",
            &unavailable,
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            None,
            None,
        )
        .expect("mutable game state must not prevent retrieving an existing paid proof");

        assert_eq!(session_id, expected);
    }

    #[test]
    fn propose_to_prove_request_builds_snark_plonk_kind() {
        let prove = propose_request().to_prove_request("devnet", false);

        assert!(!prove.retry_failed);
        match prove.proof.request {
            ProofRequestKind::SnarkPlonk(snark) => {
                assert_eq!(snark.prover_address, Address::repeat_byte(0xDD));
                assert_eq!(snark.proof.start_block_number, 4000);
                assert_eq!(snark.proof.number_of_blocks_to_prove, 1000);
                assert_eq!(snark.proof.sequence_window, None);
                assert_eq!(snark.proof.l1_head, Some(B256::repeat_byte(0x22)));
                assert_eq!(snark.proof.intermediate_root_interval, Some(100));
                assert_eq!(snark.proof.zk_artifact_hash, Some(B256::repeat_byte(0x33)));
                assert_eq!(snark.proof.zk_vm, ZkVm::Sp1);
                assert_eq!(snark.proof.zk_backend, ZkBackend::Network);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    #[test]
    fn client_redacts_endpoint_secrets_from_logs_and_errors() {
        let endpoint =
            Url::parse("https://user:password@prover.example/rpc/api-key?token=secret").unwrap();
        let client = ProofsClient::connect(&endpoint).expect("client should build");
        let source = base_prover_service_client::ProverServiceClientError::RpcTransport(
            JsonRpcClientError::Transport(
                io::Error::other(format!("request to {endpoint} failed")).into(),
            ),
        );
        let error = client.rpc_error("prover_getProof", source);
        let source = error.source().expect("RPC error should preserve a source").to_string();
        let debug = format!("{client:?}");

        assert_eq!(client.endpoint, "https://prover.example");
        for secret in ["user", "password", "api-key", "token=secret"] {
            assert!(!source.contains(secret));
            assert!(!debug.contains(secret));
        }
    }

    /// Mock requester API that returns scripted `get_proof` statuses in order,
    /// repeating the final status once the script is exhausted.
    #[derive(Clone, Debug)]
    struct MockRequesterApi {
        statuses: Arc<Mutex<VecDeque<ProofStatus>>>,
        last_status: ProofStatus,
    }

    impl MockRequesterApi {
        fn scripted<I: IntoIterator<Item = ProofStatus>>(
            statuses: I,
            last_status: ProofStatus,
        ) -> Self {
            Self { statuses: Arc::new(Mutex::new(statuses.into_iter().collect())), last_status }
        }
    }

    #[async_trait]
    impl ProverRequesterApiServer for MockRequesterApi {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> RpcResult<ProveBlockRangeResponse> {
            Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
        }

        async fn get_proof(&self, _request: GetProofRequest) -> RpcResult<GetProofResponse> {
            let status = self
                .statuses
                .lock()
                .expect("status lock should not be poisoned")
                .pop_front()
                .unwrap_or(self.last_status);
            Ok(GetProofResponse { status, error_message: None, result: None })
        }

        async fn delete_proof_request(&self, _request: DeleteProofRequest) -> RpcResult<()> {
            Err(ErrorObjectOwned::owned(
                ErrorCode::MethodNotFound.code(),
                "not used by tests",
                None::<()>,
            ))
        }

        async fn delete_proofs_by_tee_signer(
            &self,
            _request: DeleteProofsByTeeSignerRequest,
        ) -> RpcResult<u64> {
            Err(ErrorObjectOwned::owned(
                ErrorCode::MethodNotFound.code(),
                "not used by tests",
                None::<()>,
            ))
        }

        async fn list_proofs(&self, _request: ListProofsRequest) -> RpcResult<ListProofsResponse> {
            Ok(ListProofsResponse { proofs: vec![], total_count: 0 })
        }
    }

    async fn spawn_mock(api: MockRequesterApi) -> (ProofsClient, ServerHandle) {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
        let server = Server::builder().build(addr).await.expect("server should bind");
        let local_addr = server.local_addr().expect("server should have local address");
        let handle = server.start(api.into_rpc());
        let endpoint =
            Url::parse(&format!("http://{local_addr}")).expect("endpoint URL should parse");
        let client = ProofsClient::connect(&endpoint)
            .expect("client should connect")
            .with_wait_config(Duration::from_millis(5), Duration::from_millis(200));
        (client, handle)
    }

    async fn shutdown(handle: ServerHandle) {
        handle.stop().expect("server should stop");
        handle.stopped().await;
    }

    #[tokio::test]
    async fn wait_polls_until_terminal_status() {
        let api = MockRequesterApi::scripted(
            [ProofStatus::Queued, ProofStatus::Running],
            ProofStatus::Succeeded,
        );
        let (client, handle) = spawn_mock(api).await;

        let response = client
            .wait_for_completion("session-wait")
            .await
            .expect("wait should reach terminal status");

        assert_eq!(response.status, ProofStatus::Succeeded);
        shutdown(handle).await;
    }

    #[tokio::test]
    async fn wait_polls_at_deadline_when_max_wait_is_shorter_than_poll_interval() {
        let api = MockRequesterApi::scripted([ProofStatus::Running], ProofStatus::Succeeded);
        let (client, handle) = spawn_mock(api).await;
        let client = client.with_wait_config(Duration::from_secs(5), Duration::from_millis(500));

        let response = tokio::time::timeout(
            Duration::from_millis(2500),
            client.wait_for_completion("session-short-wait"),
        )
        .await
        .expect("wait should clamp the poll interval to the deadline")
        .expect("wait should make a final poll at the deadline");

        assert_eq!(response.status, ProofStatus::Succeeded);
        shutdown(handle).await;
    }

    #[tokio::test]
    async fn wait_times_out_on_non_terminal_status() {
        let api = MockRequesterApi::scripted([], ProofStatus::Running);
        let (client, handle) = spawn_mock(api).await;
        let client = client.with_wait_config(Duration::from_millis(5), Duration::from_millis(20));

        let err =
            client.wait_for_completion("session-timeout").await.expect_err("wait should time out");

        match err {
            ProofsCommandError::WaitTimeout { session_id, last_status, .. } => {
                assert_eq!(session_id, "session-timeout");
                assert_eq!(last_status, "running");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        shutdown(handle).await;
    }
}
