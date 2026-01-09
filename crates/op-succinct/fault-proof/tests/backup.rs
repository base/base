pub mod common;

use std::path::PathBuf;

use alloy_primitives::{Address, U256};
use fault_proof::{
    backup::{ProposerBackup, BACKUP_VERSION},
    contract::{GameStatus, ProposalStatus},
    proposer::Game,
};
use tempfile::TempDir;

/// Create a test game with the given index and parent.
fn test_game(index: u64, parent_index: u32) -> Game {
    Game {
        index: U256::from(index),
        address: Address::ZERO,
        parent_index,
        l2_block: U256::from(index + 100),
        status: GameStatus::IN_PROGRESS,
        proposal_status: ProposalStatus::Unchallenged,
        deadline: 0,
        should_attempt_to_resolve: false,
        should_attempt_to_claim_bond: false,
    }
}

mod validation {
    use super::*;
    use rstest::rstest;

    const M: u32 = u32::MAX;

    #[rstest]
    #[case::empty(None, &[], None, true)]
    #[case::cursor_zero_no_games(Some(0), &[], None, true)]
    #[case::single_genesis_game(Some(0), &[(0, M)], None, true)]
    #[case::chain_with_anchor(Some(1), &[(0, M), (1, 0)], Some(1), true)]
    #[case::cursor_without_games(Some(5), &[], None, false)]
    #[case::invalid_anchor_index(Some(0), &[(0, M)], Some(99), false)]
    #[case::orphaned_parent(Some(1), &[(0, M), (1, 99)], None, false)]
    fn test_validation(
        #[case] cursor: Option<u64>,
        #[case] games: &[(u64, u32)],
        #[case] anchor: Option<u64>,
        #[case] valid: bool,
    ) {
        let backup = ProposerBackup::new(
            cursor.map(U256::from),
            games.iter().map(|(idx, parent)| test_game(*idx, *parent)).collect(),
            anchor.map(U256::from),
        );

        assert_eq!(backup.validate().is_ok(), valid);
    }
}

mod persistence {
    use super::*;

    fn temp_backup_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("backup.json");
        (dir, path)
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (_dir, path) = temp_backup_path();

        let original = ProposerBackup::new(
            Some(U256::from(5)),
            vec![test_game(0, u32::MAX), test_game(1, 0), test_game(2, 1)],
            Some(U256::from(2)),
        );

        original.save(&path).unwrap();
        let loaded = ProposerBackup::load(&path).unwrap();

        assert_eq!(loaded.version, BACKUP_VERSION);
        assert_eq!(loaded.cursor, original.cursor);
        assert_eq!(loaded.games.len(), 3);
        assert_eq!(loaded.anchor_game_index, original.anchor_game_index);
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let path = PathBuf::from("/nonexistent/backup.json");
        assert!(ProposerBackup::load(&path).is_none());
    }

    #[test]
    fn load_invalid_json_returns_none() {
        let (_dir, path) = temp_backup_path();
        std::fs::write(&path, "not valid json").unwrap();
        assert!(ProposerBackup::load(&path).is_none());
    }

    #[test]
    fn load_version_mismatch_returns_none() {
        let (_dir, path) = temp_backup_path();

        let json = serde_json::json!({
            "version": BACKUP_VERSION + 1,
            "cursor": null,
            "games": [],
            "anchor_game_index": null
        });
        std::fs::write(&path, json.to_string()).unwrap();

        assert!(ProposerBackup::load(&path).is_none());
    }

    #[test]
    fn load_validation_failure_returns_none() {
        let (_dir, path) = temp_backup_path();

        let json = serde_json::json!({
            "version": BACKUP_VERSION,
            "cursor": "0x5",
            "games": [],
            "anchor_game_index": null
        });
        std::fs::write(&path, json.to_string()).unwrap();

        assert!(ProposerBackup::load(&path).is_none());
    }
}

#[cfg(feature = "integration")]
mod integration {
    use std::sync::Arc;

    use anyhow::Result;
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration};
    use tracing::info;

    use crate::common::{new_proposer, TestEnvironment};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proposer_backup_persistence() -> Result<()> {
        info!("=== Test: Proposer Backup Persistence ===");

        let env = TestEnvironment::setup().await?;

        let backup_dir = TempDir::new()?;
        let backup_path = backup_dir.path().join("proposer_backup.json");

        // Phase 1: Start proposer with backup enabled
        let proposer = Arc::new(
            new_proposer(
                &env.rpc_config,
                env.private_keys.proposer,
                &env.deployed.anchor_state_registry,
                &env.deployed.factory,
                env.game_type,
                Some(backup_path.clone()),
            )
            .await?,
        );

        let proposer_clone = proposer.clone();
        let proposer_handle = tokio::spawn(async move { proposer_clone.run().await });

        let tracked_games = env.wait_and_track_games(1, 30).await?;
        info!("Proposer created {} game(s)", tracked_games.len());

        // Wait for proposer to sync the game into its internal state
        for _ in 0..30 {
            if !proposer.state_snapshot().await.games.is_empty() {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
        let snapshot_before = proposer.state_snapshot().await;
        assert!(!snapshot_before.games.is_empty(), "Proposer should have games in state");

        // Allow backup to complete
        sleep(Duration::from_secs(2)).await;
        proposer_handle.abort();

        // Phase 2: Restart and verify backup load
        let proposer2 = Arc::new(
            new_proposer(
                &env.rpc_config,
                env.private_keys.proposer,
                &env.deployed.anchor_state_registry,
                &env.deployed.factory,
                env.game_type,
                Some(backup_path.clone()),
            )
            .await?,
        );

        proposer2.try_init().await?;

        let snapshot_after = proposer2.state_snapshot().await;

        assert!(!snapshot_after.games.is_empty(), "Restored state should contain games");
        assert_eq!(
            snapshot_before.games.len(),
            snapshot_after.games.len(),
            "Game count should match"
        );

        info!("Backup persistence test complete");
        Ok(())
    }
}
