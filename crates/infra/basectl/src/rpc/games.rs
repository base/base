//! Dispute-game discovery client for the `basectl proofs games` command group.

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_transport::TransportError;
pub use base_proof_contracts::GameStatus;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, ContractError,
    DisputeGameFactoryClient, DisputeGameFactoryContractClient, decode_create_calldata,
};
use futures::{StreamExt, stream, try_join};
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
    /// Output root claimed by the game.
    pub root_claim: B256,
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
    /// Stride between intermediate output-root checkpoints, when derivable.
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

impl GameDetails {
    /// Derives the intermediate checkpoint stride from the game's block
    /// interval and its committed root count.
    ///
    /// The factory requires the committed roots to cover every checkpoint
    /// including the final root, so `block_interval / root_count` recovers
    /// the stride. Returns `None` when the counts do not divide evenly.
    pub fn derive_intermediate_interval(block_interval: u64, root_count: usize) -> Option<u64> {
        let count = u64::try_from(root_count).ok().filter(|&c| c > 0)?;
        block_interval.is_multiple_of(count).then(|| block_interval / count)
    }
}

/// Read-only L1 client for listing and inspecting dispute games.
#[derive(Debug)]
pub struct GamesClient {
    endpoint: Url,
    provider: RootProvider,
    factory_address: Address,
    factory: DisputeGameFactoryContractClient,
    verifier: AggregateVerifierContractClient,
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
    /// Returns the factory's total game count, the matches, and whether the
    /// search was truncated (`true` when the matches came back short of
    /// `filter.limit` while games older than the scanned window remain, so
    /// older matches may exist). Scans backwards from the newest factory
    /// index with up to [`Self::SCAN_CONCURRENCY`] games in flight,
    /// preserving newest-first order, and stops after collecting
    /// `filter.limit` matches or scanning [`Self::MAX_SCAN`] games.
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
        while let Some(scanned) = scans.next().await {
            if let Some(summary) = scanned? {
                games.push(summary);
                if games.len() >= filter.limit {
                    break;
                }
            }
        }
        // A short page means the whole window was scanned; unscanned older
        // games can then silently hide matches. A full page is ordinary
        // pagination, already visible from the total-versus-listed counts.
        let truncated = games.len() < filter.limit && count > Self::MAX_SCAN as u64;
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
        if filter.missing_zk {
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
        }
        let summary = self.fetch_summary(index, at_index.game_type, at_index.proxy).await?;
        if filter.missing_zk && summary.zk_prover != Address::ZERO {
            return Ok(None);
        }
        Ok(Some(summary))
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
        )
        .map_err(|error| self.contract_error(error))?;

        let block_interval = info.l2_block_number.saturating_sub(starting_block);
        let intermediate_root_count = intermediate_roots.len();
        Ok(GameDetails {
            address,
            status,
            root_claim: info.root_claim,
            starting_block,
            target_block: info.l2_block_number,
            block_interval,
            intermediate_root_interval: GameDetails::derive_intermediate_interval(
                block_interval,
                intermediate_root_count,
            ),
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

    /// Fetches the list-row snapshot for one game.
    async fn fetch_summary(
        &self,
        index: u64,
        game_type: u32,
        address: Address,
    ) -> Result<GameSummary, ProofsCommandError> {
        let (status, info, starting_block, tee_prover, zk_prover, created_at, expected_resolution) =
            try_join!(
                self.verifier.status(address),
                self.verifier.game_info(address),
                self.verifier.starting_block_number(address),
                self.verifier.tee_prover(address),
                self.verifier.zk_prover(address),
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
            root_claim: info.root_claim,
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
        // or userinfo, which must not leak into error output.
        ProofsCommandError::L1Contract {
            endpoint: self.endpoint.origin().ascii_serialization(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_intermediate_interval_recovers_stride() {
        // 1000-block game with roots every 100 blocks commits 10 roots.
        assert_eq!(GameDetails::derive_intermediate_interval(1000, 10), Some(100));
        // Single checkpoint: the final root only.
        assert_eq!(GameDetails::derive_intermediate_interval(1000, 1), Some(1000));
    }

    #[test]
    fn derive_intermediate_interval_rejects_bad_counts() {
        assert_eq!(GameDetails::derive_intermediate_interval(1000, 0), None);
        assert_eq!(GameDetails::derive_intermediate_interval(1000, 3), None);
    }
}
