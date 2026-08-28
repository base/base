//! Dispute-game discovery client for the `basectl proofs games` command group.

use std::{
    collections::HashMap,
    fmt,
    sync::{Mutex, PoisonError},
};

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_transport::{TransportError, TransportErrorKind};
pub use base_proof_contracts::GameStatus;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, ContractError,
    DisputeGameFactoryClient, DisputeGameFactoryContractClient, ProofArtifacts,
    decode_create_calldata, encode_extra_data,
};
use futures::{StreamExt, stream, try_join};
use tracing::debug;
use url::Url;

use crate::errors::ProofsCommandError;

/// Sentinel `expectedResolution` value meaning no proof has been verified yet
/// (`type(uint64).max` on the contract).
pub const EXPECTED_RESOLUTION_NEVER: u64 = u64::MAX;

/// Filters applied by [`GamesClient::list_recent`].
#[derive(Debug, Clone, Copy)]
pub struct GameListFilter {
    /// Maximum games to return.
    pub limit: usize,
    /// Only include games of this game type.
    pub game_type: Option<u32>,
    /// Only include games whose ZK proof slot is still empty.
    pub missing_zk: bool,
}

/// Snapshot of one dispute game read from L1, sized for list rows.
#[derive(Debug, Clone, Copy)]
pub struct GameSummary {
    /// Factory index of the game.
    pub index: u64,
    /// Game proxy address.
    pub address: Address,
    /// Game type ID.
    pub game_type: u32,
    /// L1 timestamp when the game was created.
    pub created_at: u64,
    /// Current game status.
    pub status: GameStatus,
    /// Pre-state L2 block number (start of the proved range).
    pub starting_block: u64,
    /// L2 block number the game proposes (end of the proved range).
    pub target_block: u64,
    /// Address that provided a TEE proof, or zero when the slot is empty.
    pub tee_prover: Address,
    /// Address that provided a ZK proof, or zero when the slot is empty.
    pub zk_prover: Address,
    /// Timestamp at which the game can resolve, or
    /// [`EXPECTED_RESOLUTION_NEVER`] when no proof has been verified.
    pub expected_resolution: u64,
}

/// Detailed view of one dispute game, including the range parameters a ZK
/// proposal proof must match exactly.
#[derive(Debug, Clone)]
pub struct GameDetails {
    /// Game proxy address.
    pub address: Address,
    /// Current game status.
    pub status: GameStatus,
    /// Output root claimed by the game.
    pub root_claim: B256,
    /// Pre-state L2 block number (start of the proved range).
    pub starting_block: u64,
    /// L2 block number the game proposes (end of the proved range).
    pub target_block: u64,
    /// Number of L2 blocks the game covers.
    pub block_interval: u64,
    /// Canonical `INTERMEDIATE_BLOCK_INTERVAL` read from the game proxy's
    /// fixed implementation; `None` when the game does not expose it.
    pub intermediate_root_interval: Option<u64>,
    /// Number of intermediate output roots committed with the game.
    pub intermediate_root_count: usize,
    /// L1 head block hash stored at game creation time.
    pub l1_head: B256,
    /// Parent game proxy address.
    pub parent_address: Address,
    /// Address that provided a TEE proof, or zero when the slot is empty.
    pub tee_prover: Address,
    /// Address that provided a ZK proof, or zero when the slot is empty.
    pub zk_prover: Address,
    /// Number of verified proofs for this game.
    pub proof_count: u8,
    /// L1 timestamp when the game was created.
    pub created_at: u64,
    /// Timestamp at which the game can resolve, or
    /// [`EXPECTED_RESOLUTION_NEVER`] when no proof has been verified.
    pub expected_resolution: u64,
    /// 0-based index of the challenged intermediate root, when challenged.
    pub countered_index: Option<u64>,
}

/// Read-only L1 client for listing and inspecting dispute games.
pub struct GamesClient {
    endpoint: Url,
    provider: RootProvider,
    factory_address: Address,
    factory: DisputeGameFactoryContractClient,
    verifier: AggregateVerifierContractClient,
    aggregate_game_types: Mutex<HashMap<u32, bool>>,
}

impl fmt::Debug for GamesClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GamesClient")
            .field("endpoint", &self.endpoint.origin().ascii_serialization())
            .field("factory_address", &self.factory_address)
            .finish_non_exhaustive()
    }
}

impl GamesClient {
    /// Maximum factory indexes scanned backwards when listing recent games.
    pub const MAX_SCAN: usize = 256;

    /// Maximum in-flight game fetches during a list scan.
    ///
    /// Kept modest because each in-flight scan issues several L1 reads at
    /// once, and free-tier RPC endpoints rate-limit aggressively.
    const SCAN_CONCURRENCY: usize = 4;

    /// Connects a games client for the given factory address and L1 RPC URL.
    ///
    /// A single provider (and thus one HTTP connection pool) is shared by
    /// the direct RPC reads, the factory client, and the verifier client, so
    /// [`Self::SCAN_CONCURRENCY`] genuinely bounds the endpoint load.
    pub fn connect(factory: Address, l1_rpc: &Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc.clone());
        Self {
            endpoint: l1_rpc.clone(),
            factory_address: factory,
            factory: DisputeGameFactoryContractClient::new(factory, provider.clone()),
            verifier: AggregateVerifierContractClient::new(provider.clone()),
            aggregate_game_types: Mutex::new(HashMap::new()),
            provider,
        }
    }

    /// Resolves the dispute game created by an L1 factory transaction.
    ///
    /// The transaction must be a mined, successful `createWithInitData` call
    /// sent directly to the configured factory. The created game proxy is
    /// recovered via `DisputeGameFactory.games()` with the decoded calldata,
    /// so wrapped or multicall game creations are rejected.
    pub async fn game_from_creation_tx(
        &self,
        tx_hash: B256,
    ) -> Result<Address, ProofsCommandError> {
        let unresolvable =
            |reason: String| ProofsCommandError::GameFromTransaction { tx_hash, reason };

        let tx = self
            .provider
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|error| self.provider_error(error))?
            .ok_or_else(|| unresolvable("transaction not found on L1".to_string()))?;
        if tx.to() != Some(self.factory_address) {
            return Err(unresolvable(format!(
                "transaction does not call the DisputeGameFactory at {}",
                self.factory_address
            )));
        }
        let calldata = decode_create_calldata(tx.input()).ok_or_else(|| {
            unresolvable(
                "transaction is not a direct createWithInitData call; \
                 wrapped or multicall game creations are not supported"
                    .to_string(),
            )
        })?;

        let receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|error| self.provider_error(error))?
            .ok_or_else(|| unresolvable("transaction is not mined yet".to_string()))?;
        if !receipt.status() {
            return Err(unresolvable("transaction reverted, so it created no game".to_string()));
        }

        let game = self
            .factory
            .games(calldata.game_type, calldata.root_claim, calldata.extra_data)
            .await
            .map_err(|error| self.contract_error(error))?;
        if game == Address::ZERO {
            return Err(unresolvable(
                "factory has no game matching the transaction's calldata".to_string(),
            ));
        }
        Ok(game)
    }

    /// Lists recent games newest-first, applying `filter`.
    ///
    /// Returns the factory's total game count, the matches, and whether older
    /// games remain unscanned, so additional matches may exist. Scans
    /// backwards from the newest factory index with up to
    /// [`Self::SCAN_CONCURRENCY`] games in flight, preserving newest-first
    /// order, and stops after collecting `filter.limit` matches or scanning
    /// [`Self::MAX_SCAN`] games.
    pub async fn list_recent(
        &self,
        filter: GameListFilter,
    ) -> Result<(u64, Vec<GameSummary>, bool), ProofsCommandError> {
        let count = self.factory.game_count().await.map_err(|error| self.contract_error(error))?;
        let indexes = (0..count).rev().take(Self::MAX_SCAN);
        let mut scans = stream::iter(indexes)
            .map(|index| self.scan_index(index, filter))
            .buffered(Self::SCAN_CONCURRENCY);

        let mut games = Vec::new();
        let mut games_scanned = 0_u64;
        while let Some(scanned) = scans.next().await {
            games_scanned += 1;
            if let Some(summary) = scanned? {
                games.push(summary);
                if games.len() >= filter.limit {
                    break;
                }
            }
        }
        let truncated = count > games_scanned;
        Ok((count, games, truncated))
    }

    /// Fetches the game at one factory index and applies `filter`.
    ///
    /// Returns `None` when the game does not match the filter.
    async fn scan_index(
        &self,
        index: u64,
        filter: GameListFilter,
    ) -> Result<Option<GameSummary>, ProofsCommandError> {
        let at_index =
            self.factory.game_at_index(index).await.map_err(|error| self.contract_error(error))?;
        if let Some(game_type) = filter.game_type
            && at_index.game_type != game_type
        {
            return Ok(None);
        }
        if !self.is_aggregate_verifier_game_type(at_index.game_type).await? {
            return Ok(None);
        }
        let zk_prover = if filter.missing_zk {
            // Single-call precheck: the filter only needs the ZK slot, so skip
            // the full multi-read summary fetch for games that already have a
            // ZK proof.
            let zk_prover = self
                .verifier
                .zk_prover(at_index.proxy)
                .await
                .map_err(|error| self.contract_error(error))?;
            if zk_prover != Address::ZERO {
                return Ok(None);
            }
            Some(zk_prover)
        } else {
            None
        };
        let summary =
            self.fetch_summary(index, at_index.game_type, at_index.proxy, zk_prover).await?;
        Ok(Some(summary))
    }

    /// Returns whether the factory's current implementation for `game_type`
    /// exposes the `AggregateVerifier` checkpoint configuration.
    async fn is_aggregate_verifier_game_type(
        &self,
        game_type: u32,
    ) -> Result<bool, ProofsCommandError> {
        if let Some(&is_aggregate) =
            self.aggregate_game_types.lock().unwrap_or_else(PoisonError::into_inner).get(&game_type)
        {
            return Ok(is_aggregate);
        }
        // The lock is released while probing L1 so concurrent scans are not
        // serialized behind one cache miss; a duplicate probe for the same game
        // type is harmless because both insert the same value.
        let implementation =
            self.factory.game_impls(game_type).await.map_err(|error| self.contract_error(error))?;
        let is_aggregate = if implementation == Address::ZERO {
            false
        } else {
            match self.verifier.read_intermediate_block_interval(implementation).await {
                Ok(_) => true,
                Err(error) if error.is_missing_method() => {
                    // Empty reverts are indistinguishable from missing selectors; log for diagnosis.
                    debug!(game_type = %game_type, implementation = %implementation, "probe missing method; game type treated as non-aggregate");
                    false
                }
                Err(error) => return Err(self.contract_error(error)),
            }
        };
        self.aggregate_game_types
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(game_type, is_aggregate);
        Ok(is_aggregate)
    }

    /// Re-checks that the game can still accept a ZK proposal proof.
    ///
    /// Cheap two-call preflight used immediately before broadcasting
    /// `verifyProposalProof`: proof waits can span hours, so the game state
    /// read when the command started may be stale, and this refuses to spend
    /// gas on a game that has resolved or gained a ZK proof in the meantime.
    pub async fn ensure_accepts_zk_proof(
        &self,
        address: Address,
    ) -> Result<(), ProofsCommandError> {
        let (status, zk_prover) =
            try_join!(self.verifier.status(address), self.verifier.zk_prover(address))
                .map_err(|error| self.contract_error(error))?;
        let not_provable = |reason: &str| ProofsCommandError::GameNotProvable {
            game: address.to_string(),
            reason: reason.to_string(),
        };
        if status != GameStatus::InProgress {
            return Err(not_provable("game is no longer in progress"));
        }
        if zk_prover != Address::ZERO {
            return Err(not_provable("game already has a ZK proof"));
        }
        Ok(())
    }

    /// Fetches the detailed view of one game by its proxy address.
    ///
    /// The address is verified against the configured factory before returning
    /// game parameters used for proving.
    pub async fn game_details(&self, address: Address) -> Result<GameDetails, ProofsCommandError> {
        let (
            status,
            info,
            starting_block,
            l1_head,
            tee_prover,
            zk_prover,
            proof_count,
            created_at,
            expected_resolution,
            countered_plus_one,
            intermediate_roots,
            game_type,
        ) = try_join!(
            self.verifier.status(address),
            self.verifier.game_info(address),
            self.verifier.starting_block_number(address),
            self.verifier.l1_head(address),
            self.verifier.tee_prover(address),
            self.verifier.zk_prover(address),
            self.verifier.proof_count(address),
            self.verifier.created_at(address),
            self.verifier.expected_resolution(address),
            self.verifier.countered_index(address),
            self.verifier.intermediate_output_roots(address),
            self.verifier.game_type(address),
        )
        .map_err(|error| self.contract_error(error))?;

        // Verify the address against the configured factory before trusting its
        // committed range: a wrong-factory or arbitrary ABI-compatible contract
        // must not drive paid proving or an unintended L1 submission.
        let extra_data =
            encode_extra_data(info.l2_block_number, info.parent_address, &intermediate_roots);
        let registered = self
            .factory
            .games(game_type, info.root_claim, extra_data)
            .await
            .map_err(|error| self.contract_error(error))?;
        if registered != address {
            return Err(ProofsCommandError::GameNotFromFactory {
                game: address,
                factory: self.factory_address,
            });
        }

        // The game proxy delegate-calls the implementation it was cloned from
        // at creation, so this reads the stride the game was committed with.
        // The factory's current `gameImpls` entry can be swapped by an
        // implementation upgrade and must not be trusted for a live game.
        let intermediate_root_interval =
            match self.verifier.read_intermediate_block_interval(address).await {
                Ok(interval) => Some(interval),
                Err(error) if error.is_missing_method() => None,
                Err(error) => return Err(self.contract_error(error)),
            };

        let block_interval = info.l2_block_number.checked_sub(starting_block).ok_or_else(|| {
            self.contract_error(ContractError::validation(format!(
                "game target block {} precedes starting block {starting_block}",
                info.l2_block_number
            )))
        })?;
        let intermediate_root_count = intermediate_roots.len();
        Ok(GameDetails {
            address,
            status,
            root_claim: info.root_claim,
            starting_block,
            target_block: info.l2_block_number,
            block_interval,
            intermediate_root_interval,
            intermediate_root_count,
            l1_head,
            parent_address: info.parent_address,
            tee_prover,
            zk_prover,
            proof_count,
            created_at,
            expected_resolution,
            countered_index: countered_plus_one.checked_sub(1),
        })
    }

    /// Reads the immutable proof artifacts accepted by a game proxy.
    pub async fn proof_artifacts(
        &self,
        address: Address,
    ) -> Result<ProofArtifacts, ProofsCommandError> {
        self.verifier.proof_artifacts(address).await.map_err(|error| self.contract_error(error))
    }

    /// Fetches the list-row snapshot for one game.
    async fn fetch_summary(
        &self,
        index: u64,
        game_type: u32,
        address: Address,
        known_zk_prover: Option<Address>,
    ) -> Result<GameSummary, ProofsCommandError> {
        let (status, info, starting_block, tee_prover, zk_prover, created_at, expected_resolution) =
            try_join!(
                self.verifier.status(address),
                self.verifier.game_info(address),
                self.verifier.starting_block_number(address),
                self.verifier.tee_prover(address),
                async {
                    match known_zk_prover {
                        Some(zk_prover) => Ok(zk_prover),
                        None => self.verifier.zk_prover(address).await,
                    }
                },
                self.verifier.created_at(address),
                self.verifier.expected_resolution(address),
            )
            .map_err(|error| self.contract_error(error))?;

        Ok(GameSummary {
            index,
            address,
            game_type,
            created_at,
            status,
            starting_block,
            target_block: info.l2_block_number,
            tee_prover,
            zk_prover,
            expected_resolution,
        })
    }

    /// Maps an L1 provider RPC failure onto the proofs command error type.
    fn provider_error(&self, error: TransportError) -> ProofsCommandError {
        self.contract_error(ContractError::provider("L1 provider request failed", error))
    }

    /// Maps a contract read failure onto the proofs command error type.
    fn contract_error(&self, source: ContractError) -> ProofsCommandError {
        // Origin only: operator L1 URLs commonly embed API keys in the path
        // or userinfo, which must not leak into error output or its source
        // chain.
        let origin = self.endpoint.origin().ascii_serialization();
        let sanitize_transport =
            |_: TransportError| TransportErrorKind::custom_str("L1 transport request failed");
        let source = match source {
            ContractError::Call { context, source } => {
                let source = match *source {
                    alloy_contract::Error::TransportError(error) => {
                        alloy_contract::Error::TransportError(sanitize_transport(error))
                    }
                    other => other,
                };
                ContractError::call(context, source)
            }
            ContractError::Provider { context, source } => {
                ContractError::provider(context, sanitize_transport(source))
            }
            other => other,
        };
        ProofsCommandError::L1Contract { endpoint: origin, source }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, error::Error as _, sync::Mutex};

    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_provider::RootProvider;
    use alloy_rpc_client::RpcClient;
    use alloy_sol_types::SolValue;
    use alloy_transport::{TransportErrorKind, mock::Asserter};
    use base_proof_contracts::{AggregateVerifierContractClient, DisputeGameFactoryContractClient};
    use url::Url;

    use super::{GameListFilter, GamesClient};
    use crate::errors::ProofsCommandError;

    fn mocked_client(asserter: Asserter) -> GamesClient {
        let factory_address = Address::repeat_byte(0xF0);
        let provider = RootProvider::new(RpcClient::mocked(asserter));
        GamesClient {
            endpoint: Url::parse("http://localhost:8545").unwrap(),
            provider: provider.clone(),
            factory_address,
            factory: DisputeGameFactoryContractClient::new(factory_address, provider.clone()),
            verifier: AggregateVerifierContractClient::new(provider),
            aggregate_game_types: Mutex::new(HashMap::new()),
        }
    }

    fn push_abi<T: SolValue>(asserter: &Asserter, value: &T) {
        asserter.push_success(&Bytes::from(value.abi_encode()));
    }

    /// Pushes the 14 RPC responses issued by `game_details`' initial `try_join!`.
    fn push_game_details_reads(asserter: &Asserter) {
        push_abi(asserter, &U256::from(0));
        push_abi(asserter, &B256::repeat_byte(0x44));
        push_abi(asserter, &U256::from(5000));
        push_abi(asserter, &Address::repeat_byte(0x55));
        push_abi(asserter, &U256::from(4000));
        push_abi(asserter, &B256::repeat_byte(0x66));
        push_abi(asserter, &Address::repeat_byte(0x77));
        push_abi(asserter, &Address::ZERO);
        push_abi(asserter, &U256::from(1));
        push_abi(asserter, &1_700_000_000_u64);
        push_abi(asserter, &u64::MAX);
        push_abi(asserter, &U256::ZERO);
        push_abi(asserter, &Bytes::from(B256::repeat_byte(0x77).to_vec()));
        push_abi(asserter, &2_u32);
    }

    #[test]
    fn l1_transport_errors_redact_endpoint_secrets_from_source_chain() {
        let mut client = mocked_client(Asserter::new());
        client.endpoint =
            Url::parse("https://user:password@l1.example/v3/api-key?token=secret").unwrap();
        let transport = TransportErrorKind::custom_str(&format!(
            "request to {} failed: connection refused",
            client.endpoint
        ));

        let error = client.provider_error(transport);
        let message = error.to_string();
        let source = error.source().expect("L1 error should preserve a source").to_string();
        let debug = format!("{client:?}");

        assert!(message.contains("https://l1.example"));
        assert!(source.contains("L1 transport request failed"));
        for secret in ["user", "password", "api-key", "token=secret"] {
            assert!(!source.contains(secret));
            assert!(!debug.contains(secret));
        }
    }

    #[tokio::test]
    async fn heterogeneous_game_types_skip_unsupported_and_cache_aggregate_probe() {
        let asserter = Asserter::new();
        let client = mocked_client(asserter.clone());
        let unsupported_game = Address::repeat_byte(0x11);
        let unsupported_impl = Address::repeat_byte(0x12);
        let aggregate_impl = Address::repeat_byte(0x22);

        push_abi(&asserter, &(1_u32, 1_u64, unsupported_game));
        push_abi(&asserter, &unsupported_impl);
        asserter.push_success(&Bytes::new());
        push_abi(&asserter, &aggregate_impl);
        push_abi(&asserter, &U256::from(100));

        let skipped = client
            .scan_index(0, GameListFilter { limit: 1, game_type: None, missing_zk: false })
            .await
            .unwrap();
        assert!(skipped.is_none());
        assert!(client.is_aggregate_verifier_game_type(2).await.unwrap());
        assert!(client.is_aggregate_verifier_game_type(2).await.unwrap());
        assert!(asserter.read_q().is_empty(), "cached type should not be probed twice");
    }

    #[tokio::test]
    async fn aggregate_probe_propagates_rpc_failures() {
        let asserter = Asserter::new();
        let client = mocked_client(asserter.clone());

        push_abi(&asserter, &Address::repeat_byte(0x22));
        asserter.push_failure_msg("RPC unavailable");

        let error = client.is_aggregate_verifier_game_type(2).await.unwrap_err();
        assert!(matches!(error, ProofsCommandError::L1Contract { .. }));
    }

    #[tokio::test]
    async fn fetch_summary_reuses_known_zk_prover() {
        let asserter = Asserter::new();
        let client = mocked_client(asserter.clone());
        let game = Address::repeat_byte(0x33);

        push_abi(&asserter, &U256::ZERO);
        push_abi(&asserter, &alloy_primitives::B256::repeat_byte(0x44));
        push_abi(&asserter, &U256::from(5000));
        push_abi(&asserter, &Address::repeat_byte(0x55));
        push_abi(&asserter, &U256::from(4000));
        push_abi(&asserter, &Address::repeat_byte(0x66));
        push_abi(&asserter, &1_700_000_000_u64);
        push_abi(&asserter, &u64::MAX);

        let summary = client.fetch_summary(7, 2, game, Some(Address::ZERO)).await.unwrap();

        assert_eq!(summary.zk_prover, Address::ZERO);
        assert!(asserter.read_q().is_empty(), "known ZK prover should avoid a second RPC call");
    }

    #[tokio::test]
    async fn game_details_rejects_game_not_registered_with_factory() {
        let asserter = Asserter::new();
        let client = mocked_client(asserter.clone());
        let game = Address::repeat_byte(0xAA);
        let factory = Address::repeat_byte(0xF0);

        push_game_details_reads(&asserter);
        push_abi(&asserter, &(Address::ZERO, 0_u64));

        let error = client.game_details(game).await.unwrap_err();
        assert!(matches!(
            error,
            ProofsCommandError::GameNotFromFactory { game: err_game, factory: err_factory }
                if err_game == game && err_factory == factory
        ));
        assert!(asserter.read_q().is_empty(), "stride read should not run after factory rejection");
    }

    #[tokio::test]
    async fn game_details_reads_stride_from_game_proxy_not_factory_registration() {
        let asserter = Asserter::new();
        let client = mocked_client(asserter.clone());
        let game = Address::repeat_byte(0xAA);

        push_game_details_reads(&asserter);
        push_abi(&asserter, &(game, 0_u64));
        push_abi(&asserter, &U256::from(100));

        let details = client.game_details(game).await.unwrap();

        assert_eq!(details.address, game);
        assert_eq!(details.target_block, 5000);
        assert_eq!(details.starting_block, 4000);
        assert_eq!(details.block_interval, 1000);
        assert_eq!(details.intermediate_root_interval, Some(100));
        assert_eq!(details.intermediate_root_count, 1);
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn game_details_tolerates_game_without_interval_method() {
        let asserter = Asserter::new();
        let client = mocked_client(asserter.clone());
        let game = Address::repeat_byte(0xAA);

        push_game_details_reads(&asserter);
        push_abi(&asserter, &(game, 0_u64));
        asserter.push_success(&Bytes::new());

        let details = client.game_details(game).await.unwrap();

        assert_eq!(details.intermediate_root_interval, None);
        assert!(asserter.read_q().is_empty());
    }
}
