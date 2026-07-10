//! Anchor root update management.

use std::sync::Arc;

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorRoot, AnchorSnapshot, AnchorStateRegistryClient,
    DisputeGameFactoryClient, GameStatus, encode_set_anchor_state_calldata, game_lookup_blocks,
    game_lookup_key,
};
use base_proof_rpc::L2Provider;
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::{BondTransactionSubmitter, ChallengerMetrics, OutputValidator};

/// Best-effort updater for the `AnchorStateRegistry`.
pub struct AnchorUpdater {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    anchor_registry_client: Arc<dyn AnchorStateRegistryClient>,
    output_validator: OutputValidator<dyn L2Provider>,
    cached_next_game: Option<(AnchorSnapshot, Address)>,
    anchor_state_registry_address: Address,
    game_type: u32,
    block_interval: u64,
    intermediate_block_interval: u64,
}

impl std::fmt::Debug for AnchorUpdater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnchorUpdater")
            .field("anchor_state_registry_address", &self.anchor_state_registry_address)
            .field("game_type", &self.game_type)
            .field("block_interval", &self.block_interval)
            .field("intermediate_block_interval", &self.intermediate_block_interval)
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
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> Self {
        Self {
            factory_client,
            anchor_registry_client,
            output_validator: OutputValidator::new(l2_provider),
            cached_next_game: None,
            anchor_state_registry_address,
            game_type,
            block_interval,
            intermediate_block_interval,
        }
    }

    /// Finds the next game after the anchor game and advances the anchor root if it is ready.
    pub async fn poll(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
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
                let Some(address) = self.next_game(anchor.anchor_root, anchor.anchor_game).await
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

    async fn next_game(&self, anchor_root: AnchorRoot, anchor_game: Address) -> Option<Address> {
        let blocks = match game_lookup_blocks(
            anchor_root.l2_block_number,
            self.block_interval,
            self.intermediate_block_interval,
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
            self.block_interval,
            self.intermediate_block_interval,
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

    async fn try_update(
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
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
        MockAggregateVerifier, MockAnchorStateRegistry, MockBondTransactionSubmitter,
        MockDisputeGameFactory, MockL2Provider, addr, build_test_header_and_account, mock_state,
    };

    const ASR_ADDRESS: Address = Address::new([0xAA; 20]);
    const GAME_TYPE: u32 = 1;
    const BLOCK_INTERVAL: u64 = 100;
    const INTERMEDIATE_BLOCK_INTERVAL: u64 = 100;

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
            BLOCK_INTERVAL,
            INTERMEDIATE_BLOCK_INTERVAL,
        )
    }

    #[tokio::test]
    async fn poll_updates_next_defender_win() {
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = ASR_ADDRESS;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(tx_hash)]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, game);
        assert_eq!(calls[0].1, ASR_ADDRESS);
    }

    #[tokio::test]
    async fn poll_waits_for_in_progress_next_game() {
        let game = addr(1);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let state = mock_state(GameStatus::InProgress, Address::ZERO, 100);

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn poll_reuses_cached_next_game_while_anchor_is_unchanged() {
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let state = mock_state(GameStatus::InProgress, Address::ZERO, 100);

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state.clone())]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(tx_hash)]);
        let mut updater = updater(Arc::clone(&factory), anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;
        factory.uuid_games.lock().unwrap().clear();

        let mut resolved_state = state;
        resolved_state.status = GameStatus::DefenderWins;
        resolved_state.anchor_state_registry = ASR_ADDRESS;
        verifier.update_game(game, resolved_state);

        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, game);
    }

    #[tokio::test]
    async fn poll_clears_cached_next_game_after_successful_update() {
        let game = addr(1);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = ASR_ADDRESS;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter =
            MockBondTransactionSubmitter::with_responses(vec![Ok(tx_hash), Ok(tx_hash)]);
        let mut updater = updater(Arc::clone(&factory), anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;
        factory.uuid_games.lock().unwrap().clear();
        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, game);
    }

    #[tokio::test]
    async fn poll_stops_at_challenger_wins_next_game() {
        let challenger_win = addr(10);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, challenger_win);

        let verifier = MockAggregateVerifier::new(HashMap::from([(
            challenger_win,
            mock_state(GameStatus::ChallengerWins, Address::ZERO, 100),
        )]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        let mut updater = updater(Arc::clone(&factory), anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;
        factory.uuid_games.lock().unwrap().clear();
        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert_eq!(verifier.status_read_count(challenger_win), 1);
    }

    #[tokio::test]
    async fn poll_starts_after_current_anchor_game() {
        let anchor_game = addr(10);
        let next_game = addr(11);
        let tx_hash = B256::repeat_byte(0xDD);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(anchor_game));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, anchor_game, output_root, next_game);
        let mut next_state = mock_state(GameStatus::DefenderWins, Address::ZERO, 200);
        next_state.anchor_state_registry = ASR_ADDRESS;

        let verifier = MockAggregateVerifier::new(HashMap::from([
            (anchor_game, mock_state(GameStatus::DefenderWins, Address::ZERO, 100)),
            (next_game, next_state),
        ]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(tx_hash)]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, next_game);
    }

    #[tokio::test]
    async fn poll_waits_for_finalized_defender_win() {
        let game = addr(1);
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
        let mut l2 = MockL2Provider::new();
        let output_root = insert_l2_block(&mut l2, BLOCK_INTERVAL);
        insert_next_game(&factory, ASR_ADDRESS, output_root, game);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = ASR_ADDRESS;
        state.is_finalized = false;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        let mut updater = updater(factory, anchor_registry, Arc::new(l2));

        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
    }
}
