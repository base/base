//! Simple file-based state persistence for proposer recovery.
//!
//! On restart, the proposer can restore its cursor and game cache from a backup file,
//! avoiding a full re-sync from the factory contract.

use std::{collections::HashSet, io::Write, path::Path};

use tempfile::NamedTempFile;

use alloy_primitives::U256;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::proposer::Game;

/// Current backup format version. Increment when making breaking changes.
pub const BACKUP_VERSION: u32 = 1;

/// Serializable backup of the proposer state.
#[derive(Serialize, Deserialize)]
pub struct ProposerBackup {
    pub version: u32,
    pub cursor: Option<U256>,
    pub games: Vec<Game>,
    pub anchor_game_index: Option<U256>,
}

impl ProposerBackup {
    /// Create a new backup with the current version.
    pub fn new(cursor: Option<U256>, games: Vec<Game>, anchor_game_index: Option<U256>) -> Self {
        Self { version: BACKUP_VERSION, cursor, games, anchor_game_index }
    }

    /// Validate backup integrity.
    ///
    /// Checks for:
    /// - Cursor exists but no games (likely stale/corrupted)
    /// - Anchor game index references a non-existent game
    /// - Games with parent indices that don't exist in the backup (orphaned games)
    pub fn validate(&self) -> Result<()> {
        // Check: cursor exists but no games
        if let Some(cursor) = self.cursor {
            if self.games.is_empty() && cursor > U256::ZERO {
                bail!("cursor exists but no games");
            }
        }

        // Check: anchor game index references non-existent game
        if let Some(anchor_idx) = self.anchor_game_index {
            if !self.games.iter().any(|g| g.index == anchor_idx) {
                bail!("anchor game index references non-existent game");
            }
        }

        // Check: games with orphaned parent references (parent_index == u32::MAX means genesis)
        let game_indices: HashSet<U256> = self.games.iter().map(|g| g.index).collect();
        if self.games.iter().any(|g| {
            g.parent_index != u32::MAX && !game_indices.contains(&U256::from(g.parent_index))
        }) {
            bail!("games have orphaned parent references");
        }

        Ok(())
    }

    /// Save the backup to a file as JSON (atomic via temp file + rename with fsync).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("failed to serialize proposer backup")?;

        let dir = path.parent().unwrap_or(Path::new("."));
        let mut temp =
            NamedTempFile::new_in(dir).context("failed to create proposer backup temp file")?;
        temp.write_all(json.as_bytes()).context("failed to write proposer backup temp file")?;
        temp.as_file().sync_all().context("failed to sync proposer backup temp file")?;
        temp.persist(path).context("failed to persist proposer backup file")?;

        tracing::debug!(?path, games = self.games.len(), "Proposer state backed up");
        Ok(())
    }

    /// Load and validate a backup from file.
    ///
    /// Returns None and logs a warning if:
    /// - File doesn't exist or can't be read
    /// - JSON parsing fails
    /// - Version mismatch
    /// - Validation fails (stale/corrupted data)
    pub fn load(path: &Path) -> Option<Self> {
        let json = std::fs::read_to_string(path).ok()?;

        let backup = match serde_json::from_str::<Self>(&json) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?path, error = %e, "Failed to parse backup, starting fresh");
                return None;
            }
        };

        if backup.version != BACKUP_VERSION {
            tracing::warn!(
                ?path,
                backup_version = backup.version,
                current_version = BACKUP_VERSION,
                "Backup version mismatch, starting fresh"
            );
            return None;
        }

        if let Err(e) = backup.validate() {
            tracing::warn!(?path, error = %e, "Backup validation failed, starting fresh");
            return None;
        }

        tracing::info!(?path, games = backup.games.len(), "Proposer backup loaded");
        Some(backup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Schema guard: if this test fails, you likely need to bump BACKUP_VERSION.
    /// This catches accidental schema changes that would break backup compatibility.
    #[test]
    fn backup_schema_guard() {
        use crate::contract::{GameStatus, ProposalStatus};
        use alloy_primitives::Address;

        // If Game fields change, this won't compile or the JSON keys will differ
        let game = Game {
            index: U256::ZERO,
            address: Address::ZERO,
            parent_index: 0,
            l2_block: U256::ZERO,
            status: GameStatus::IN_PROGRESS,
            proposal_status: ProposalStatus::Unchallenged,
            deadline: 0,
            should_attempt_to_resolve: false,
            should_attempt_to_claim_bond: false,
        };

        let json = serde_json::to_value(&game).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();

        // If this assertion fails, Game schema changed - bump BACKUP_VERSION!
        assert_eq!(
            keys,
            vec![
                "address",
                "deadline",
                "index",
                "l2_block",
                "parent_index",
                "proposal_status",
                "should_attempt_to_claim_bond",
                "should_attempt_to_resolve",
                "status",
            ],
            "Game schema changed! Bump BACKUP_VERSION in backup.rs"
        );

        // Check ProposerBackup fields
        let backup = ProposerBackup::new(None, vec![], None);
        let json = serde_json::to_value(&backup).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();

        assert_eq!(
            keys,
            vec!["anchor_game_index", "cursor", "games", "version"],
            "ProposerBackup schema changed! Bump BACKUP_VERSION in backup.rs"
        );
    }
}
