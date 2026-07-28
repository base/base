//! Prover-service requester client helpers for the `basectl proofs` command group.

use std::{num::NonZeroU64, time::Duration};

use alloy_primitives::{Address, B256};
use base_prover_service_client::{
    ProofRequesterClient, ProverServiceClientConfig, ProverServiceClientError,
};
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofRequest,
    ProofRequestKind, ProofSessionId, ProofStatus, ProveBlockRangeRequest, SnarkPlonkProofRequest,
    ZkBackend, ZkProofRequest, ZkVm,
};
use tokio::time::{Instant, sleep};
use tracing::{debug, info};
use url::Url;

use crate::{
    errors::ProofsCommandError,
    rpc::games::{GameDetails, GameStatus},
};

/// Parameters for a `basectl proofs finalize` compressed ZK proof request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofFinalizeRequest {
    /// First L2 block number to prove.
    pub start_block: NonZeroU64,
    /// Number of consecutive L2 blocks to prove.
    pub num_blocks: u64,
    /// ZK proving backend that executes the proof.
    pub zk_backend: ZkBackend,
    /// Explicit session ID override. When `None`, an idempotent session ID is
    /// derived from the network name, ZK backend, and block range.
    pub session_id: Option<String>,
    /// Optional L1 head hash used for witness generation.
    pub l1_head: Option<B256>,
    /// Optional sequencing window.
    pub sequence_window: Option<u64>,
    /// Optional intermediate output root interval.
    pub intermediate_root_interval: Option<u64>,
}

impl ProofFinalizeRequest {
    /// Session ID namespace for proofs requested via basectl.
    const SESSION_NAMESPACE: &'static [u8] = b"basectl";

    /// Returns the session ID proof subtype for the selected ZK backend.
    ///
    /// Each backend derives distinct session IDs so the same block range can
    /// be proved on different backends without colliding on the
    /// prover-service idempotency key.
    const fn session_subtype(&self) -> &'static str {
        match self.zk_backend {
            ZkBackend::DryRun => "zk/sp1/compressed/dry_run",
            ZkBackend::Cluster => "zk/sp1/compressed",
            ZkBackend::Network => "zk/sp1/compressed/network",
        }
    }

    /// Returns the effective session ID for `network`.
    ///
    /// Uses the explicit override when set; otherwise derives an idempotent
    /// `UUIDv5` from the network name, ZK backend, and block range so
    /// re-running the same command resolves to the same prover-service
    /// session instead of enqueueing a duplicate proof.
    pub fn effective_session_id(&self, network: &str) -> String {
        self.session_id.clone().unwrap_or_else(|| {
            let pre_state_block = self.start_block.get() - 1;
            ProofSessionId::derive_from_components(
                Self::SESSION_NAMESPACE,
                self.session_subtype(),
                &[
                    network.as_bytes(),
                    &pre_state_block.to_be_bytes(),
                    &self.num_blocks.to_be_bytes(),
                ],
            )
        })
    }

    /// Builds the prover-service prove-block-range request for `network`, deriving
    /// its effective session ID with [`Self::effective_session_id`].
    pub fn to_prove_request(&self, network: &str) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: self.effective_session_id(network),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: self.start_block.get() - 1,
                    number_of_blocks_to_prove: self.num_blocks,
                    sequence_window: self.sequence_window,
                    l1_head: self.l1_head,
                    intermediate_root_interval: self.intermediate_root_interval,
                    zk_vm: ZkVm::Sp1,
                    zk_backend: self.zk_backend,
                }),
            },
        }
    }
}

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
    /// Explicit session ID override. When `None`, an idempotent session ID is
    /// derived from the network name, game, block range, and prover address.
    pub session_id: Option<String>,
}

impl ProofProposeRequest {
    /// Session ID namespace for proofs requested via basectl.
    const SESSION_NAMESPACE: &'static [u8] = b"basectl";

    /// Derives the idempotent proposal-proof session ID shared by
    /// `basectl proofs propose` and `basectl proofs submit`.
    ///
    /// Both commands must agree on this derivation so `submit` can find the
    /// session that `propose` created from the same game and submitter
    /// wallet without the operator copying session IDs around.
    pub fn derive_session_id(
        network: &str,
        zk_backend: ZkBackend,
        game: Address,
        pre_state_block: u64,
        num_blocks: u64,
        prover_address: Address,
    ) -> String {
        ProofSessionId::derive_from_components(
            Self::SESSION_NAMESPACE,
            Self::session_subtype_for(zk_backend),
            &[
                network.as_bytes(),
                game.as_slice(),
                &pre_state_block.to_be_bytes(),
                &num_blocks.to_be_bytes(),
                prover_address.as_slice(),
            ],
        )
    }

    /// Returns the session ID proof subtype for a ZK backend.
    const fn session_subtype_for(zk_backend: ZkBackend) -> &'static str {
        match zk_backend {
            ZkBackend::DryRun => "zk/sp1/snark_plonk/dry_run",
            ZkBackend::Cluster => "zk/sp1/snark_plonk",
            ZkBackend::Network => "zk/sp1/snark_plonk/network",
        }
    }

    /// Builds a game-matched request from the game's on-chain state.
    ///
    /// Validates that the game can still accept a ZK proposal proof and takes
    /// the block range, L1 head, and checkpoint stride from the game so the
    /// proof journal matches what the contract reconstructs.
    /// `intermediate_root_interval` overrides the stride derived from the
    /// game's committed roots.
    pub fn for_game(
        details: &GameDetails,
        prover_address: Address,
        zk_backend: ZkBackend,
        session_id: Option<String>,
        intermediate_root_interval: Option<u64>,
    ) -> Result<Self, ProofsCommandError> {
        let not_provable = |reason: &str| ProofsCommandError::GameNotProvable {
            game: details.address.to_string(),
            reason: reason.to_string(),
        };
        if details.status != GameStatus::InProgress {
            return Err(not_provable("game is not in progress"));
        }
        if !details.missing_zk() {
            return Err(not_provable("game already has a ZK proof"));
        }
        if details.block_interval == 0 {
            return Err(not_provable("game covers an empty block range"));
        }
        let intermediate_root_interval =
            intermediate_root_interval.or(details.intermediate_root_interval).ok_or_else(|| {
                not_provable(
                    "cannot derive the intermediate root interval from the game's \
                     committed roots; pass --intermediate-root-interval",
                )
            })?;
        Ok(Self {
            game: details.address,
            pre_state_block: details.starting_block,
            num_blocks: details.block_interval,
            l1_head: details.l1_head,
            intermediate_root_interval,
            prover_address,
            zk_backend,
            session_id,
        })
    }

    /// Returns the effective session ID for `network`.
    ///
    /// Uses the explicit override when set; otherwise derives an idempotent
    /// `UUIDv5` from the network name, game address, block range, and prover
    /// address so re-running the same command resolves to the same
    /// prover-service session instead of enqueueing a duplicate proof.
    pub fn effective_session_id(&self, network: &str) -> String {
        self.session_id.clone().unwrap_or_else(|| {
            Self::derive_session_id(
                network,
                self.zk_backend,
                self.game,
                self.pre_state_block,
                self.num_blocks,
                self.prover_address,
            )
        })
    }

    /// Builds the prover-service prove-block-range request for `network`, deriving
    /// its effective session ID with [`Self::effective_session_id`].
    pub fn to_prove_request(&self, network: &str) -> ProveBlockRangeRequest {
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
                        zk_vm: ZkVm::Sp1,
                        zk_backend: self.zk_backend,
                    },
                    prover_address: self.prover_address,
                }),
            },
        }
    }
}

/// Prover-service requester client used by the `basectl proofs` commands.
#[derive(Debug, Clone)]
pub struct ProofsClient {
    endpoint: Url,
    requester: ProofRequesterClient,
    poll_interval: Duration,
    max_wait: Duration,
}

impl ProofsClient {
    /// Connects a requester client to the prover-service `endpoint`.
    pub fn connect(endpoint: &Url) -> Result<Self, ProofsCommandError> {
        let config = ProverServiceClientConfig::new(endpoint.as_str());
        let requester = ProofRequesterClient::connect(&config).map_err(|source| {
            ProofsCommandError::BuildClient { endpoint: endpoint.to_string(), source }
        })?;
        Ok(Self {
            endpoint: endpoint.clone(),
            requester,
            poll_interval: config.poll_interval(),
            max_wait: config.max_wait(),
        })
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
        ProofsCommandError::Rpc { endpoint: self.endpoint.to_string(), method, source }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        net::SocketAddr,
        num::NonZeroU64,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::{Address, B256};
    use base_prover_service_protocol::{
        DeleteProofRequest, DeleteProofsByTeeSignerRequest, GetProofRequest, GetProofResponse,
        ListProofsRequest, ListProofsResponse, ProofRequestKind, ProofSessionId, ProofStatus,
        ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer, ZkBackend, ZkVm,
    };
    use jsonrpsee::{
        core::{RpcResult, async_trait},
        server::{Server, ServerHandle},
        types::{ErrorObjectOwned, error::ErrorCode},
    };
    use url::Url;

    use super::{ProofFinalizeRequest, ProofProposeRequest, ProofsClient};
    use crate::{
        errors::ProofsCommandError,
        rpc::games::{GameDetails, GameStatus},
    };

    fn finalize_request() -> ProofFinalizeRequest {
        ProofFinalizeRequest {
            start_block: NonZeroU64::new(100).unwrap(),
            num_blocks: 5,
            zk_backend: ZkBackend::Cluster,
            session_id: None,
            l1_head: None,
            sequence_window: None,
            intermediate_root_interval: None,
        }
    }

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
            None,
            None,
        )
        .expect("provable game should build a request")
    }

    #[test]
    fn session_id_is_deterministic_per_network_and_range() {
        let request = finalize_request();

        assert_eq!(
            request.effective_session_id("mainnet"),
            request.effective_session_id("mainnet")
        );
        assert_ne!(
            request.effective_session_id("mainnet"),
            request.effective_session_id("sepolia")
        );

        let other_range = ProofFinalizeRequest {
            start_block: NonZeroU64::new(101).unwrap(),
            ..finalize_request()
        };
        assert_ne!(
            request.effective_session_id("mainnet"),
            other_range.effective_session_id("mainnet")
        );
    }

    #[test]
    fn session_id_uses_protocol_pre_state_block() {
        let request = finalize_request();
        let pre_state_block = 99_u64.to_be_bytes();
        let num_blocks = 5_u64.to_be_bytes();
        let expected = ProofSessionId::derive_from_components(
            b"basectl",
            "zk/sp1/compressed",
            &[b"mainnet", &pre_state_block, &num_blocks],
        );

        assert_eq!(request.effective_session_id("mainnet"), expected);
    }

    #[test]
    fn session_id_is_distinct_per_backend() {
        let cluster = finalize_request();
        let network = ProofFinalizeRequest { zk_backend: ZkBackend::Network, ..cluster.clone() };
        let dry_run = ProofFinalizeRequest { zk_backend: ZkBackend::DryRun, ..cluster.clone() };

        let ids = [
            cluster.effective_session_id("mainnet"),
            network.effective_session_id("mainnet"),
            dry_run.effective_session_id("mainnet"),
        ];
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
        assert_ne!(ids[1], ids[2]);
    }

    #[test]
    fn to_prove_request_propagates_each_backend() {
        for backend in [ZkBackend::Cluster, ZkBackend::Network, ZkBackend::DryRun] {
            let request = ProofFinalizeRequest { zk_backend: backend, ..finalize_request() };
            match request.to_prove_request("devnet").proof.request {
                ProofRequestKind::Compressed(zk) => assert_eq!(zk.zk_backend, backend),
                other => panic!("unexpected proof request kind: {other:?}"),
            }
        }
    }

    #[test]
    fn explicit_session_id_overrides_derivation() {
        let request = ProofFinalizeRequest {
            session_id: Some("custom-session".to_string()),
            ..finalize_request()
        };

        assert_eq!(request.effective_session_id("mainnet"), "custom-session");
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
                None,
                None,
            )
            .expect_err("unprovable game should be rejected");
            assert!(matches!(error, ProofsCommandError::GameNotProvable { .. }));
        }
    }

    #[test]
    fn propose_for_game_accepts_stride_override() {
        let no_stride = GameDetails { intermediate_root_interval: None, ..provable_game() };
        let request = ProofProposeRequest::for_game(
            &no_stride,
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            None,
            Some(250),
        )
        .expect("stride override should build a request");

        assert_eq!(request.intermediate_root_interval, 250);
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

        let base_id = request.effective_session_id("mainnet");
        assert_ne!(base_id, other_game.effective_session_id("mainnet"));
        assert_ne!(base_id, other_prover.effective_session_id("mainnet"));
        assert_ne!(base_id, other_backend.effective_session_id("mainnet"));
        assert_ne!(base_id, request.effective_session_id("sepolia"));
    }

    #[test]
    fn derive_session_id_matches_effective_session_id() {
        let request = propose_request();

        assert_eq!(
            request.effective_session_id("mainnet"),
            ProofProposeRequest::derive_session_id(
                "mainnet",
                request.zk_backend,
                request.game,
                request.pre_state_block,
                request.num_blocks,
                request.prover_address,
            )
        );
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
    fn propose_to_prove_request_builds_snark_plonk_kind() {
        let prove = propose_request().to_prove_request("devnet");

        match prove.proof.request {
            ProofRequestKind::SnarkPlonk(snark) => {
                assert_eq!(snark.prover_address, Address::repeat_byte(0xDD));
                assert_eq!(snark.proof.start_block_number, 4000);
                assert_eq!(snark.proof.number_of_blocks_to_prove, 1000);
                assert_eq!(snark.proof.sequence_window, None);
                assert_eq!(snark.proof.l1_head, Some(B256::repeat_byte(0x22)));
                assert_eq!(snark.proof.intermediate_root_interval, Some(100));
                assert_eq!(snark.proof.zk_vm, ZkVm::Sp1);
                assert_eq!(snark.proof.zk_backend, ZkBackend::Network);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
        }
    }

    #[test]
    fn to_prove_request_maps_all_fields() {
        let request = ProofFinalizeRequest {
            start_block: NonZeroU64::new(100).unwrap(),
            num_blocks: 5,
            zk_backend: ZkBackend::Network,
            session_id: Some("session-map".to_string()),
            l1_head: Some(alloy_primitives::B256::repeat_byte(0xaa)),
            sequence_window: Some(3600),
            intermediate_root_interval: Some(10),
        };

        let prove = request.to_prove_request("devnet");
        assert_eq!(prove.proof.session_id, "session-map");
        match prove.proof.request {
            ProofRequestKind::Compressed(zk) => {
                assert_eq!(zk.start_block_number, 99);
                assert_eq!(zk.number_of_blocks_to_prove, 5);
                assert_eq!(zk.sequence_window, Some(3600));
                assert_eq!(zk.l1_head, Some(alloy_primitives::B256::repeat_byte(0xaa)));
                assert_eq!(zk.intermediate_root_interval, Some(10));
                assert_eq!(zk.zk_vm, ZkVm::Sp1);
                assert_eq!(zk.zk_backend, ZkBackend::Network);
            }
            other => panic!("unexpected proof request kind: {other:?}"),
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
    async fn submit_returns_accepted_session_id() {
        let api = MockRequesterApi::scripted([], ProofStatus::Queued);
        let (client, handle) = spawn_mock(api).await;

        let request = finalize_request();
        let session_id =
            client.submit(request.to_prove_request("devnet")).await.expect("submit should succeed");

        assert_eq!(session_id, finalize_request().effective_session_id("devnet"));
        shutdown(handle).await;
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
