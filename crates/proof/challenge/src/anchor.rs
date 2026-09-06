//! Anchor root update management.

use std::sync::Arc;

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorRoot, AnchorSnapshot, AnchorStateRegistryClient,
    DisputeGameFactoryClient, GameStatus, encode_set_anchor_state_calldata, game_lookup_blocks,
    game_lookup_key, resolve_intervals,
};
use base_proof_rpc::L2Provider;
use base_tx_manager::TxManager;
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::{ChallengeSubmitter, ChallengerMetrics, OutputValidator};

/// Best-effort updater for the `AnchorStateRegistry`.
pub struct AnchorUpdater {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    anchor_registry_client: Arc<dyn AnchorStateRegistryClient>,
    output_validator: OutputValidator<dyn L2Provider>,
    cached_next_game: Option<(AnchorSnapshot, Address)>,
    anchor_state_registry_address: Address,
    game_type: u32,
}

impl std::fmt::Debug for AnchorUpdater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnchorUpdater")
            .field("anchor_state_registry_address", &self.anchor_state_registry_address)
            .field("game_type", &self.game_type)
            .finish_non_exhaustive()
    }
}

impl AnchorUpdater {
    /// Creates an anchor updater.
    pub fn new(
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        anchor_registry_client: Arc<dyn AnchorStateRegistryClient>,
        l2_provider: Arc<dyn L2Provider>,
        anchor_state_registry_address: Address,
        game_type: u32,
    ) -> Self {
        Self {
            factory_client,
            anchor_registry_client,
            output_validator: OutputValidator::new(l2_provider),
            cached_next_game: None,
            anchor_state_registry_address,
            game_type,
        }
    }

    /// Finds the next game after the anchor game and advances the anchor root if it is ready.
    pub async fn poll<T: TxManager>(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) {
        let anchor = match self.anchor_registry_client.anchor_snapshot().await {
            Ok(anchor) => anchor,
            Err(e) => {
                warn!(error = %e, "failed to read anchor snapshot for anchor update");
                return;
            }
        };

        let game_address = match self.cached_next_game {
            Some((cached_anchor, address)) if cached_anchor == anchor => address,
            _ => {
                self.cached_next_game = None;
                let Some(address) =
                    self.next_game(verifier_client, anchor.anchor_root, anchor.anchor_game).await
                else {
                    return;
                };
                self.cached_next_game = Some((anchor, address));
                address
            }
        };

        match verifier_client.status(game_address).await {
            Ok(GameStatus::DefenderWins) => {}
            Ok(GameStatus::InProgress) => {
                debug!(game = %game_address, "anchor update waiting for next game");
                return;
            }
            Ok(status) => {
                debug!(
                    game = %game_address,
                    status = %status,
                    "next game cannot update anchor"
                );
                self.cached_next_game = None;
                return;
            }
            Err(e) => {
                warn!(game = %game_address, error = %e, "failed to read next game status for anchor update");
                return;
            }
        }

        if Self::try_update(game_address, verifier_client, submitter).await {
            self.cached_next_game = None;
        }
    }

    async fn next_game(
        &self,
        verifier_client: &dyn AggregateVerifierClient,
        anchor_root: AnchorRoot,
        anchor_game: Address,
    ) -> Option<Address> {
        // Resolved from the anchor block, which is where the next game's range starts:
        // the verifier switches to a shorter cadence at the Cobalt activation block, so a
        // pair cached at startup silently stops matching any game past the boundary.
        let (block_interval, intermediate_block_interval) = match resolve_intervals(
            self.factory_client.as_ref(),
            verifier_client,
            self.game_type,
            anchor_root.l2_block_number,
        )
        .await
        {
            Ok(intervals) => intervals,
            Err(e) => {
                warn!(error = %e, "failed to resolve anchor update intervals");
                return None;
            }
        };

        let blocks = match game_lookup_blocks(
            anchor_root.l2_block_number,
            block_interval,
            intermediate_block_interval,
        ) {
            Ok(blocks) => blocks,
            Err(e) => {
                warn!(error = %e, "invalid anchor update lookup blocks");
                return None;
            }
        };

        let block_count = blocks.len();
        let mut roots =
            stream::iter(blocks)
                .map(|block| async move {
                    (block, self.output_validator.compute_output_root(block).await)
                })
                .buffered(OutputValidator::<dyn L2Provider>::VALIDATION_CONCURRENCY);

        let mut intermediate_roots = Vec::with_capacity(block_count);
        while let Some((block, result)) = roots.next().await {
            match result {
                Ok(root) => intermediate_roots.push(root),
                Err(e) => {
                    debug!(block, error = %e, "anchor update waiting for output root");
                    return None;
                }
            }
        }

        let parent = if anchor_game == Address::ZERO {
            self.anchor_state_registry_address
        } else {
            anchor_game
        };
        let key = match game_lookup_key(
            anchor_root.l2_block_number,
            parent,
            block_interval,
            intermediate_block_interval,
            &intermediate_roots,
        ) {
            Ok(key) => key,
            Err(e) => {
                warn!(error = %e, "failed to build anchor update game lookup key");
                return None;
            }
        };

        match self.factory_client.games(self.game_type, key.root_claim, key.extra_data).await {
            Ok(Address::ZERO) => {
                debug!(
                    target_block = key.target_block,
                    parent = %parent,
                    output_root = %key.root_claim,
                    "next anchor game not found"
                );
                None
            }
            Ok(address) => Some(address),
            Err(e) => {
                warn!(
                    target_block = key.target_block,
                    parent = %parent,
                    output_root = %key.root_claim,
                    error = %e,
                    "failed to look up next anchor game"
                );
                None
            }
        }
    }

    async fn try_update<T: TxManager>(
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> bool {
        let asr_address = match verifier_client.anchor_state_registry(game_address).await {
            Ok(address) => address,
            Err(e) => {
                warn!(game = %game_address, error = %e, "failed to read anchor registry for game");
                return false;
            }
        };

        match verifier_client.is_game_finalized(asr_address, game_address).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(game = %game_address, asr = %asr_address, "anchor update waiting for finality");
                return false;
            }
            Err(e) => {
                warn!(game = %game_address, asr = %asr_address, error = %e, "failed to read game finality for anchor update");
                return false;
            }
        }

        let preflight = match verifier_client.anchor_preflight(asr_address, game_address).await {
            Ok(preflight) => preflight,
            Err(e) => {
                warn!(game = %game_address, asr = %asr_address, error = %e, "failed to read anchor preflight");
                return false;
            }
        };

        if preflight.permanently_ineligible() {
            // Later games are keyed from their parent game, so re-running the same-anchor lookup
            // would rediscover this game. External anchor advancement is required to move past it.
            info!(
                game = %game_address,
                asr = %asr_address,
                blacklisted = preflight.blacklisted,
                retired = preflight.retired,
                "skipping permanently ineligible anchor update"
            );
            ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
                .increment(1);
            return false;
        }

        if preflight.paused || !preflight.respected {
            debug!(
                game = %game_address,
                asr = %asr_address,
                paused = preflight.paused,
                respected = preflight.respected,
                "anchor update waiting for registry eligibility"
            );
            return false;
        }

        let game_info = match verifier_client.game_info(game_address).await {
            Ok(info) => info,
            Err(e) => {
                warn!(game = %game_address, asr = %asr_address, error = %e, "failed to read game info for anchor update");
                return false;
            }
        };

        if game_info.l2_block_number <= preflight.anchor_root.l2_block_number {
            info!(
                game = %game_address,
                asr = %asr_address,
                game_l2_block = game_info.l2_block_number,
                anchor_l2_block = preflight.anchor_root.l2_block_number,
                "skipping stale anchor update"
            );
            ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
                .increment(1);
            return false;
        }

        let calldata = encode_set_anchor_state_calldata(game_address);
        match submitter.send_bond_tx(game_address, asr_address, calldata).await {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    asr = %asr_address,
                    tx_hash = %tx_hash,
                    "anchor state registry updated"
                );
                ChallengerMetrics::anchor_update_tx_outcome_total(
                    ChallengerMetrics::STATUS_SUCCESS,
                )
                .increment(1);
                ChallengerMetrics::anchor_l2_block_number().set(game_info.l2_block_number as f64);
                true
            }
            Err(e) => {
                warn!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "anchor update transaction failed"
                );
                ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use alloy_primitives::B256;
    use base_protocol::OutputRoot;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockGameState,
        MockL2Provider, MockTxManager, addr, build_test_header_and_account, mock_state,
        receipt_with_status,
    };

    const ASR_ADDRESS: Address = Address::new([0xAA; 20]);
    const GAME_TYPE: u32 = 1;
    const BLOCK_INTERVAL: u64 = 100;
    const INTERMEDIATE_BLOCK_INTERVAL: u64 = 100;
    const FAST_ACTIVATION_BLOCK: u64 = 200;
    const FAST_BLOCK_INTERVAL: u64 = 50;
    const FAST_INTERMEDIATE_BLOCK_INTERVAL: u64 = 25;

    fn insert_l2_block(l2: &mut MockL2Provider, block: u64) -> B256 {
        let storage_hash = B256::repeat_byte(block as u8);
        let (header, account) = build_test_header_and_account(block, storage_hash);
        let output_root =
            OutputRoot::from_parts(header.state_root, storage_hash, header.hash_slow()).hash();
        l2.insert_block(block, header, account);
        output_root
    }

    fn insert_next_game(
        factory: &MockDisputeGameFactory,
        parent: Address,
        output_root: B256,
        game: Address,
    ) {
        let extra_data =
            game_lookup_key(0, parent, BLOCK_INTERVAL, INTERMEDIATE_BLOCK_INTERVAL, &[output_root])
                .unwrap()
                .extra_data;
        factory.insert_uuid_game(GAME_TYPE, output_root, extra_data, game);
    }

    /// The mock verifier resolves the intervals the anchor tests build their games with.
    fn verifier(games: HashMap<Address, MockGameState>) -> MockAggregateVerifier {
        MockAggregateVerifier::new(games)
            .with_intervals(BLOCK_INTERVAL, INTERMEDIATE_BLOCK_INTERVAL)
    }

    fn tx_success(tx_hash: B256) -> base_tx_manager::SendResponse {
        Ok(receipt_with_status(true, tx_hash))
    }

    fn submitter(
        responses: Vec<base_tx_manager::SendResponse>,
    ) -> (ChallengeSubmitter<MockTxManager>, MockTxManager) {
        let tx_manager = MockTxManager::with_responses(responses);
        (ChallengeSubmitter::new(tx_manager.clone()), tx_manager)
    }

    fn updater(
        factory: Arc<MockDisputeGameFactory>,
        anchor_registry: Arc<MockAnchorStateRegistry>,
        l2: Arc<MockL2Provider>,
    ) -> AnchorUpdater {
        AnchorUpdater::new(
            factory as Arc<dyn DisputeGameFactoryClient>,
            anchor_registry as Arc<dyn AnchorStateRegistryClient>,
            l2 as Arc<dyn L2Provider>,
            ASR_ADDRESS,
            GAME_TYPE,
        )
    }

    #[tokio::test]
    async fn poll_updates_next_defender_win() {
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = ASR_ADDRESS;

        let verifier = verifier(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![tx_success(tx_hash)]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tx_data, encode_set_anchor_state_calldata(game));
        assert_eq!(calls[0].to.unwrap(), ASR_ADDRESS);
    }

    #[tokio::test]
    async fn poll_waits_for_in_progress_next_game() {
        let game = addr(1);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let state = mock_state(GameStatus::InProgress, Address::ZERO, 100);

        let verifier = verifier(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        assert!(tx_manager.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn poll_reuses_cached_next_game_while_anchor_is_unchanged() {
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let state = mock_state(GameStatus::InProgress, Address::ZERO, 100);

        let verifier = verifier(HashMap::from([(game, state.clone())]));
        let (submitter, tx_manager) = submitter(vec![tx_success(tx_hash)]);
        let mut updater = updater(Arc::clone(&factory), anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;
        factory.uuid_games.lock().unwrap().clear();

        let mut resolved_state = state;
        resolved_state.status = GameStatus::DefenderWins;
        resolved_state.anchor_state_registry = ASR_ADDRESS;
        verifier.update_game(game, resolved_state);

        updater.poll(&verifier, &submitter).await;

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tx_data, encode_set_anchor_state_calldata(game));
    }

    #[tokio::test]
    async fn poll_clears_cached_next_game_after_successful_update() {
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = ASR_ADDRESS;

        let verifier = verifier(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![tx_success(tx_hash), tx_success(tx_hash)]);
        let mut updater = updater(Arc::clone(&factory), anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;
        factory.uuid_games.lock().unwrap().clear();
        updater.poll(&verifier, &submitter).await;

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tx_data, encode_set_anchor_state_calldata(game));
    }

    #[tokio::test]
    async fn poll_stops_at_challenger_wins_next_game() {
        let challenger_win = addr(10);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, challenger_win);

        let verifier = verifier(HashMap::from([(
            challenger_win,
            mock_state(GameStatus::ChallengerWins, Address::ZERO, 100),
        )]));
        let (submitter, tx_manager) = submitter(vec![]);
        let mut updater = updater(Arc::clone(&factory), anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;
        factory.uuid_games.lock().unwrap().clear();
        updater.poll(&verifier, &submitter).await;

        assert!(tx_manager.recorded_calls().is_empty());
        assert_eq!(verifier.status_read_count(challenger_win), 1);
    }

    #[tokio::test]
    async fn poll_starts_after_current_anchor_game() {
        let anchor_game = addr(10);
        let next_game = addr(11);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(anchor_game));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, anchor_game, output_root, next_game);
        let mut next_state = mock_state(GameStatus::DefenderWins, Address::ZERO, 200);
        next_state.anchor_state_registry = ASR_ADDRESS;

        let verifier = verifier(HashMap::from([
            (anchor_game, mock_state(GameStatus::DefenderWins, Address::ZERO, 100)),
            (next_game, next_state),
        ]));
        let (submitter, tx_manager) = submitter(vec![tx_success(tx_hash)]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tx_data, encode_set_anchor_state_calldata(next_game));
    }

    #[tokio::test]
    async fn poll_waits_for_finalized_defender_win() {
        let game = addr(1);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::default();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = ASR_ADDRESS;
        state.is_finalized = false;

        let verifier = verifier(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        assert!(tx_manager.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn poll_finds_next_game_built_with_fast_intervals() {
        // The anchor sits exactly at the Cobalt activation block, so the next game
        // spans FAST_BLOCK_INTERVAL and carries two intermediate roots instead of
        // one. Its UUID is only reproducible with the post-activation pair.
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        anchor_registry.snapshot.lock().unwrap().anchor_root.l2_block_number =
            FAST_ACTIVATION_BLOCK;

        let mut l2 = MockL2Provider::default();
        let roots: Vec<B256> = (1..=FAST_BLOCK_INTERVAL / FAST_INTERMEDIATE_BLOCK_INTERVAL)
            .map(|i| {
                insert_l2_block(
                    &mut l2,
                    FAST_ACTIVATION_BLOCK + i * FAST_INTERMEDIATE_BLOCK_INTERVAL,
                )
            })
            .collect();
        // Present so the slow-cadence run fails on a UUID mismatch rather than on a
        // missing output root.
        insert_l2_block(&mut l2, FAST_ACTIVATION_BLOCK + BLOCK_INTERVAL);
        let l2 = Arc::new(l2);

        let extra_data = game_lookup_key(
            FAST_ACTIVATION_BLOCK,
            ASR_ADDRESS,
            FAST_BLOCK_INTERVAL,
            FAST_INTERMEDIATE_BLOCK_INTERVAL,
            &roots,
        )
        .unwrap()
        .extra_data;
        factory.insert_uuid_game(GAME_TYPE, *roots.last().unwrap(), extra_data, game);

        let mut state = mock_state(
            GameStatus::DefenderWins,
            Address::ZERO,
            FAST_ACTIVATION_BLOCK + FAST_BLOCK_INTERVAL,
        );
        state.anchor_state_registry = ASR_ADDRESS;
        let games = HashMap::from([(game, state)]);

        let cobalt_verifier = verifier(games.clone()).with_fast_intervals(
            FAST_ACTIVATION_BLOCK,
            FAST_BLOCK_INTERVAL,
            FAST_INTERMEDIATE_BLOCK_INTERVAL,
        );
        let (cobalt_submitter, tx_manager) = submitter(vec![tx_success(tx_hash)]);
        let mut cobalt_updater =
            updater(Arc::clone(&factory), Arc::clone(&anchor_registry), Arc::clone(&l2));
        cobalt_updater.poll(&cobalt_verifier, &cobalt_submitter).await;

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1, "anchor should advance to the post-activation game");
        assert_eq!(calls[0].tx_data, encode_set_anchor_state_calldata(game));

        // Same fixture against a verifier that never switches cadence — the pair the
        // old startup-cached updater would have used. It must not find the game.
        let (stale_submitter, tx_manager) = submitter(vec![tx_success(tx_hash)]);
        let mut stale_updater = updater(factory, anchor_registry, l2);
        stale_updater.poll(&verifier(games), &stale_submitter).await;

        assert!(
            tx_manager.recorded_calls().is_empty(),
            "slow-cadence intervals build a different UUID, so no game is found"
        );
    }
}
