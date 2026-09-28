//! Sync-on-startup thresholds and the caught-up check.

use std::time::Duration;

use base_consensus_engine::EngineState;
use base_protocol::L2BlockInfo;

/// Thresholds for deciding when a syncing sequencer has caught up to the canonical chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOnStartupConfig {
    /// Maximum safe head age. An isolated fork keeps a fresh unsafe tip but a stale safe head,
    /// so this is the signal that separates the canonical chain from a local fork.
    pub max_safe_age: Duration,
    /// Maximum unsafe head age.
    pub max_unsafe_lag: Duration,
    /// Time allowed to catch up before the node exits with an error. [`None`] waits forever.
    pub timeout: Option<Duration>,
}

impl SyncOnStartupConfig {
    /// Default [`Self::max_safe_age`].
    pub const DEFAULT_MAX_SAFE_AGE: Duration = Duration::from_secs(900);
    /// Default [`Self::max_unsafe_lag`].
    pub const DEFAULT_MAX_UNSAFE_LAG: Duration = Duration::from_secs(60);
    /// Interval between caught-up checks.
    pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

    /// Returns whether `progress` shows a node that followed the canonical chain to a fresh head.
    ///
    /// While syncing, the sequencer is stopped and the engine runs as a validator, which sets
    /// `el_sync_finished` only from a gossip or derivation update. The unsafe head therefore came
    /// from the canonical chain, never from the local fork seeded at startup.
    pub fn is_caught_up(&self, progress: &SyncProgress) -> bool {
        progress.el_sync_finished
            && progress.safe_age <= self.max_safe_age
            && progress.unsafe_lag <= self.max_unsafe_lag
    }
}

impl Default for SyncOnStartupConfig {
    fn default() -> Self {
        Self {
            max_safe_age: Self::DEFAULT_MAX_SAFE_AGE,
            max_unsafe_lag: Self::DEFAULT_MAX_UNSAFE_LAG,
            timeout: None,
        }
    }
}

/// A snapshot of engine sync progress relative to wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncProgress {
    /// Whether the EL confirmed a forkchoice update.
    pub el_sync_finished: bool,
    /// Current unsafe head.
    pub unsafe_head: L2BlockInfo,
    /// Wall-clock age of the safe head.
    pub safe_age: Duration,
    /// Wall-clock age of the unsafe head.
    pub unsafe_lag: Duration,
}

impl SyncProgress {
    /// Observes `state` at `now`, measured since the Unix epoch.
    pub fn observe(state: &EngineState, now: Duration) -> Self {
        let age =
            |head: L2BlockInfo| now.saturating_sub(Duration::from_secs(head.block_info.timestamp));
        let unsafe_head = state.sync_state.unsafe_head();
        Self {
            el_sync_finished: state.el_sync_finished,
            unsafe_head,
            safe_age: age(state.sync_state.safe_head()),
            unsafe_lag: age(unsafe_head),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_protocol::BlockInfo;
    use rstest::rstest;

    use super::*;

    const NOW: Duration = Duration::from_secs(10_000);

    fn progress(safe_age: u64, unsafe_lag: u64) -> SyncProgress {
        SyncProgress {
            el_sync_finished: true,
            unsafe_head: L2BlockInfo::default(),
            safe_age: Duration::from_secs(safe_age),
            unsafe_lag: Duration::from_secs(unsafe_lag),
        }
    }

    #[rstest]
    #[case::both_fresh(progress(30, 2), true)]
    #[case::at_thresholds(progress(900, 60), true)]
    #[case::stale_safe_fresh_unsafe(progress(901, 2), false)]
    #[case::fresh_safe_stale_unsafe(progress(30, 61), false)]
    #[case::el_syncing(SyncProgress { el_sync_finished: false, ..progress(30, 2) }, false)]
    fn caught_up_requires_every_condition(#[case] progress: SyncProgress, #[case] expected: bool) {
        assert_eq!(SyncOnStartupConfig::default().is_caught_up(&progress), expected);
    }

    #[test]
    fn observe_measures_head_ages_from_block_timestamps() {
        let mut state = EngineState::default();
        let head = |timestamp| L2BlockInfo {
            block_info: BlockInfo { timestamp, ..Default::default() },
            ..Default::default()
        };
        state.sync_state =
            state.sync_state.apply_update(base_consensus_engine::EngineSyncStateUpdate {
                unsafe_head: Some(head(NOW.as_secs() - 2)),
                safe_head: Some(head(NOW.as_secs() - 300)),
                ..Default::default()
            });
        state.el_sync_finished = true;

        let progress = SyncProgress::observe(&state, NOW);

        assert!(progress.el_sync_finished);
        assert_eq!(progress.unsafe_lag, Duration::from_secs(2));
        assert_eq!(progress.safe_age, Duration::from_secs(300));
    }

    #[test]
    fn observe_treats_an_unset_safe_head_as_stale() {
        let progress = SyncProgress::observe(&EngineState::default(), NOW);

        assert_eq!(progress.safe_age, NOW);
        assert!(!SyncOnStartupConfig::default().is_caught_up(&progress));
    }
}
