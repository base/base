//! Test utilities: mock stubs for contract clients and scanner tests.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use base_proof_contracts::{
    AggregateVerifierClient, ContractError, DisputeGameFactoryClient, GameAtIndex, GameInfo,
};

use crate::scanner::{GameScanner, ScannerConfig};

/// Per-game state for the mock verifier.
#[derive(Debug, Clone)]
pub(crate) struct MockGameState {
    /// Game status (`0=IN_PROGRESS`, `1=CHALLENGER_WINS`, `2=DEFENDER_WINS`).
    pub status: u8,
    /// Address of the ZK prover (`Address::ZERO` if unchallenged).
    pub zk_prover: Address,
    /// Game info (root claim, L2 block number, parent index).
    pub game_info: GameInfo,
    /// Starting block number for this game.
    pub starting_block_number: u64,
}

/// Mock dispute game factory with configurable per-index game data.
pub(crate) struct MockDisputeGameFactory {
    /// Ordered list of games in the factory.
    pub games: Vec<GameAtIndex>,
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.games.len() as u64)
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        self.games
            .get(index as usize)
            .cloned()
            .ok_or_else(|| ContractError::Validation(format!("index {index} out of bounds")))
    }

    async fn init_bonds(&self, _game_type: u32) -> Result<U256, ContractError> {
        Ok(U256::ZERO)
    }

    async fn game_impls(&self, _game_type: u32) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
}

/// Mock aggregate verifier with configurable per-address game state.
pub(crate) struct MockAggregateVerifier {
    /// Per-address game state lookup.
    pub games: HashMap<Address, MockGameState>,
}

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.game_info.clone())
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn status(&self, game_address: Address) -> Result<u8, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.status)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.zk_prover)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError> {
        self.games
            .get(&game_address)
            .map(|s| s.starting_block_number)
            .ok_or_else(|| ContractError::Validation(format!("unknown game {game_address}")))
    }

    async fn read_block_interval(&self, _impl_address: Address) -> Result<u64, ContractError> {
        Ok(10)
    }

    async fn read_intermediate_block_interval(
        &self,
        _impl_address: Address,
    ) -> Result<u64, ContractError> {
        Ok(5)
    }
}

/// Helper to create an address from a `u64` index.
fn addr(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..20].copy_from_slice(&index.to_be_bytes());
    Address::from(bytes)
}

/// Helper to build a factory game entry.
fn factory_game(index: u64, game_type: u32) -> GameAtIndex {
    GameAtIndex { game_type, timestamp: 1_000_000 + index, proxy: addr(index) }
}

/// Helper to build mock game state for the verifier.
fn mock_state(status: u8, zk_prover: Address, block_number: u64) -> MockGameState {
    MockGameState {
        status,
        zk_prover,
        game_info: GameInfo {
            root_claim: B256::repeat_byte(block_number as u8),
            l2_block_number: block_number,
            parent_index: 0,
        },
        starting_block_number: block_number.saturating_sub(10),
    }
}

/// Mock factory that returns an error for specific indices.
pub(crate) struct ErrorOnIndexFactory {
    /// The inner factory providing normal game data.
    pub inner: MockDisputeGameFactory,
    /// Indices that should return an error when queried.
    pub error_indices: Vec<u64>,
}

#[async_trait]
impl DisputeGameFactoryClient for ErrorOnIndexFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        self.inner.game_count().await
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        if self.error_indices.contains(&index) {
            return Err(ContractError::Validation(format!("simulated error at index {index}")));
        }
        self.inner.game_at_index(index).await
    }

    async fn init_bonds(&self, game_type: u32) -> Result<U256, ContractError> {
        self.inner.init_bonds(game_type).await
    }

    async fn game_impls(&self, game_type: u32) -> Result<Address, ContractError> {
        self.inner.game_impls(game_type).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET_TYPE: u32 = 1;

    /// Happy path: mixed games, only matching / `IN_PROGRESS` / unchallenged returned.
    #[tokio::test]
    async fn test_scan_happy_path() {
        // Game 0: matching type, IN_PROGRESS, unchallenged -> candidate
        // Game 1: wrong type -> skipped
        // Game 2: matching type, status=1 (not in progress) -> skipped
        // Game 3: matching type, IN_PROGRESS, already challenged -> skipped
        // Game 4: matching type, IN_PROGRESS, unchallenged -> candidate
        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![
                factory_game(0, TARGET_TYPE),
                factory_game(1, 99),
                factory_game(2, TARGET_TYPE),
                factory_game(3, TARGET_TYPE),
                factory_game(4, TARGET_TYPE),
            ],
        });

        let challenger_addr = Address::repeat_byte(0xCC);
        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        verifier_games.insert(addr(2), mock_state(1, Address::ZERO, 200));
        verifier_games.insert(addr(3), mock_state(0, challenger_addr, 300));
        verifier_games.insert(addr(4), mock_state(0, Address::ZERO, 400));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        // last_scanned=None, start = max(0, 5-1000) = 0, so games 0..=4 scanned
        // Game 0: candidate. Game 1: wrong type. Game 2: status != 0.
        // Game 3: challenged. Game 4: candidate.
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[0].l2_block_number, 100);
        assert_eq!(candidates[1].index, 4);
        assert_eq!(candidates[1].l2_block_number, 400);
        assert_eq!(new_last_scanned, Some(4));
    }

    /// Already-challenged games (zkProver != zero) are filtered out.
    #[tokio::test]
    async fn test_scan_filters_challenged_games() {
        let challenger_addr = Address::repeat_byte(0xAA);

        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![
                factory_game(0, TARGET_TYPE),
                factory_game(1, TARGET_TYPE),
                factory_game(2, TARGET_TYPE),
            ],
        });

        let mut verifier_games = HashMap::new();
        // All IN_PROGRESS but index 0 and 2 are already challenged
        verifier_games.insert(addr(0), mock_state(0, challenger_addr, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));
        verifier_games.insert(addr(2), mock_state(0, challenger_addr, 300));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        // Scan from the beginning (last_scanned=None, lookback covers all)
        // start = max(0, 3-1000) = 0, end = 2
        // Game 0: challenged -> skip. Game 1: unchallenged -> candidate. Game 2: challenged -> skip.
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(new_last_scanned, Some(2));
    }

    /// Games with non-matching `game_type` are skipped.
    #[tokio::test]
    async fn test_scan_filters_wrong_game_type() {
        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, 99), factory_game(1, 50), factory_game(2, TARGET_TYPE)],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(2), mock_state(0, Address::ZERO, 100));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        // start = max(0, 3-1000) = 0, end = 2
        // Game 0: wrong type. Game 1: wrong type. Game 2: matching -> candidate.
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 2);
        assert_eq!(new_last_scanned, Some(2));
    }

    /// Empty factory returns empty vec without error.
    #[tokio::test]
    async fn test_scan_empty_factory() {
        let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
        let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert!(candidates.is_empty());
        assert_eq!(new_last_scanned, None);
    }

    /// No new games since last scan returns empty vec.
    #[tokio::test]
    async fn test_scan_no_new_games() {
        let factory = Arc::new(MockDisputeGameFactory {
            games: vec![factory_game(0, TARGET_TYPE), factory_game(1, TARGET_TYPE)],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        // last_scanned = Some(1) (gameCount - 1), so start = 2 > end = 1
        let (candidates, new_last_scanned) = scanner.scan(Some(1)).await.unwrap();

        assert!(candidates.is_empty());
        assert_eq!(new_last_scanned, Some(1));
    }

    /// Lookback window: on fresh start with large factory, only `lookback_games` are scanned.
    #[tokio::test]
    async fn test_scan_lookback_window() {
        // Factory with 100 games, but lookback is 3 -> only scan indices 97, 98, 99
        let mut games = Vec::new();
        let mut verifier_games = HashMap::new();

        for i in 0..100u64 {
            games.push(factory_game(i, TARGET_TYPE));
            verifier_games.insert(addr(i), mock_state(0, Address::ZERO, i * 10));
        }

        let factory = Arc::new(MockDisputeGameFactory { games });
        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 3 },
        );

        // Fresh start: last_scanned = None
        // start = max(0, 100-3) = 97, end = 99
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 97);
        assert_eq!(candidates[1].index, 98);
        assert_eq!(candidates[2].index, 99);
        assert_eq!(new_last_scanned, Some(99));
    }

    /// Error resilience: a per-game error is logged and skipped, other games still returned.
    /// `new_last_scanned` is set to one before the errored index so it will be retried.
    #[tokio::test]
    async fn test_scan_skips_errored_games() {
        // 3 games: index 1 will error, indices 0 and 2 are valid candidates
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory {
                games: vec![
                    factory_game(0, TARGET_TYPE),
                    factory_game(1, TARGET_TYPE),
                    factory_game(2, TARGET_TYPE),
                ],
            },
            error_indices: vec![1],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        // index 1 won't be queried on the verifier because the factory errors first
        verifier_games.insert(addr(2), mock_state(0, Address::ZERO, 300));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        // start = max(0, 3-1000) = 0, end = 2
        // Index 0 -> candidate. Index 1 errors -> skipped. Index 2 -> candidate.
        // new_last_scanned = lowest_error(1) - 1 = 0, so next scan retries from index 1.
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 2);
        assert_eq!(new_last_scanned, Some(0));
    }

    /// Retry: errored games are retried on the next scan when the error clears.
    #[tokio::test]
    async fn test_scan_retries_errored_games() {
        // Phase 1: index 1 errors, so new_last_scanned = Some(0)
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory {
                games: vec![
                    factory_game(0, TARGET_TYPE),
                    factory_game(1, TARGET_TYPE),
                    factory_game(2, TARGET_TYPE),
                ],
            },
            error_indices: vec![1],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(0, Address::ZERO, 100));
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));
        verifier_games.insert(addr(2), mock_state(0, Address::ZERO, 300));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games.clone() });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        let (_, new_last_scanned) = scanner.scan(None).await.unwrap();
        assert_eq!(new_last_scanned, Some(0));

        // Phase 2: no errors, pass last_scanned = Some(0) to retry from index 1
        let factory2 = Arc::new(MockDisputeGameFactory {
            games: vec![
                factory_game(0, TARGET_TYPE),
                factory_game(1, TARGET_TYPE),
                factory_game(2, TARGET_TYPE),
            ],
        });

        let verifier2 = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner2 = GameScanner::new(
            factory2,
            verifier2,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        let (candidates, new_last_scanned) = scanner2.scan(Some(0)).await.unwrap();

        // Indices 1 and 2 are now scanned successfully
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(candidates[1].index, 2);
        assert_eq!(new_last_scanned, Some(2));
    }

    /// Error at the first index (0) with `last_scanned = None` preserves fresh-start semantics.
    #[tokio::test]
    async fn test_scan_error_at_first_index() {
        let factory = Arc::new(ErrorOnIndexFactory {
            inner: MockDisputeGameFactory {
                games: vec![factory_game(0, TARGET_TYPE), factory_game(1, TARGET_TYPE)],
            },
            error_indices: vec![0],
        });

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(1), mock_state(0, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let scanner = GameScanner::new(
            factory,
            verifier,
            ScannerConfig { game_type: TARGET_TYPE, lookback_games: 1000 },
        );

        // last_scanned = None, lowest_error = 0 -> preserves None (fresh-start semantics)
        let (candidates, new_last_scanned) = scanner.scan(None).await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
        assert_eq!(new_last_scanned, None);
    }
}
