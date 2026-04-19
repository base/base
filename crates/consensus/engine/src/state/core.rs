//! The internal state of the engine controller.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_rpc_types_engine::ForkchoiceState;
use base_protocol::L2BlockInfo;
use serde::{Deserialize, Serialize};

use crate::Metrics;

/// The synchronization state of the execution layer across different safety levels.
///
/// Tracks block progression through various stages of verification and finalization,
/// from initial unsafe blocks received via P2P to fully finalized blocks derived from
/// finalized L1 data. Each level represents increasing confidence in the block's validity.
///
/// # Safety Levels
///
/// The state tracks blocks at different safety levels, listed from least to most safe:
///
/// 1. **Unsafe** - Most recent blocks from P2P network (unverified)
/// 2. **Safe** - Derived from L1 data
/// 3. **Finalized** - Derived from finalized L1 data only
///
/// See the [Base specifications](https://specs.base.org) for detailed safety definitions.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct EngineSyncState {
    /// Most recent block found on the P2P network (lowest safety level).
    unsafe_head: L2BlockInfo,
    /// Derived from L1 data.
    safe_head: L2BlockInfo,
    /// Derived from finalized L1 data with only finalized dependencies (highest safety level).
    finalized_head: L2BlockInfo,
}

impl EngineSyncState {
    /// Returns the current unsafe head.
    pub const fn unsafe_head(&self) -> L2BlockInfo {
        self.unsafe_head
    }

    /// Returns the current safe head.
    pub const fn safe_head(&self) -> L2BlockInfo {
        self.safe_head
    }

    /// Returns the current finalized head.
    pub const fn finalized_head(&self) -> L2BlockInfo {
        self.finalized_head
    }

    /// Creates a `ForkchoiceState`
    ///
    /// - `head_block` = `unsafe_head`
    /// - `safe_block` = `safe_head`
    /// - `finalized_block` = `finalized_head`
    ///
    /// If the block info is not yet available, the default values are used.
    pub const fn create_forkchoice_state(&self) -> ForkchoiceState {
        ForkchoiceState {
            head_block_hash: self.unsafe_head.hash(),
            safe_block_hash: self.safe_head.hash(),
            finalized_block_hash: self.finalized_head.hash(),
        }
    }

    /// Applies the update to the provided sync state, using the current state values if the update
    /// is not specified. Returns the new sync state.
    pub fn apply_update(self, sync_state_update: EngineSyncStateUpdate) -> Self {
        let now_secs =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        if let Some(unsafe_head) = sync_state_update.unsafe_head {
            Metrics::block_labels(Metrics::UNSAFE_BLOCK_LABEL)
                .set(unsafe_head.block_info.number as f64);
            Metrics::block_refs_latency(Metrics::UNSAFE_BLOCK_LABEL)
                .set(now_secs - unsafe_head.block_info.timestamp as f64);
        }
        if let Some(safe_head) = sync_state_update.safe_head {
            Metrics::block_labels(Metrics::SAFE_BLOCK_LABEL)
                .set(safe_head.block_info.number as f64);
            Metrics::block_refs_latency(Metrics::SAFE_BLOCK_LABEL)
                .set(now_secs - safe_head.block_info.timestamp as f64);
        }
        if let Some(finalized_head) = sync_state_update.finalized_head {
            Metrics::block_labels(Metrics::FINALIZED_BLOCK_LABEL)
                .set(finalized_head.block_info.number as f64);
            Metrics::block_refs_latency(Metrics::FINALIZED_BLOCK_LABEL)
                .set(now_secs - finalized_head.block_info.timestamp as f64);
        }

        Self {
            unsafe_head: sync_state_update.unsafe_head.unwrap_or(self.unsafe_head),
            safe_head: sync_state_update.safe_head.unwrap_or(self.safe_head),
            finalized_head: sync_state_update.finalized_head.unwrap_or(self.finalized_head),
        }
    }
}

/// Specifies how to update the sync state of the engine.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSyncStateUpdate {
    /// Most recent block found on the p2p network
    pub unsafe_head: Option<L2BlockInfo>,
    /// Derived from L1 data.
    pub safe_head: Option<L2BlockInfo>,
    /// Derived from finalized L1 data.
    pub finalized_head: Option<L2BlockInfo>,
}

/// The chain state viewed by the engine controller.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct EngineState {
    /// The sync state of the engine.
    pub sync_state: EngineSyncState,

    /// Whether or not the EL has finished syncing.
    pub el_sync_finished: bool,

    /// Track when the rollup node changes the forkchoice to restore previous
    /// known unsafe chain. e.g. Unsafe Reorg caused by Invalid span batch.
    /// This update does not retry except engine returns non-input error
    /// because engine may forgot backupUnsafeHead or backupUnsafeHead is not part
    /// of the chain.
    pub need_fcu_call_backup_unsafe_reorg: bool,
}

impl EngineState {
    /// Returns if consolidation is needed.
    ///
    /// [Consolidation] is only performed by a rollup node when the unsafe head
    /// is ahead of the safe head. When the two are equal, consolidation isn't
    /// required and the [`crate::BuildTask`] can be used to build the block.
    ///
    /// [Consolidation]: https://specs.base.org/protocol/consensus/derivation#l1-consolidation-payload-attributes-matching
    pub fn needs_consolidation(&self) -> bool {
        self.sync_state.safe_head() != self.sync_state.unsafe_head()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "metrics")]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(feature = "metrics")]
    use base_protocol::BlockInfo;
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;
    #[cfg(feature = "metrics")]
    use rstest::rstest;

    use super::*;

    impl EngineState {
        /// Set the unsafe head.
        pub fn set_unsafe_head(&mut self, unsafe_head: L2BlockInfo) {
            self.sync_state = self.sync_state.apply_update(EngineSyncStateUpdate {
                unsafe_head: Some(unsafe_head),
                ..Default::default()
            });
        }

        /// Set the safe head.
        pub fn set_safe_head(&mut self, safe_head: L2BlockInfo) {
            self.sync_state = self.sync_state.apply_update(EngineSyncStateUpdate {
                safe_head: Some(safe_head),
                ..Default::default()
            });
        }

        /// Set the finalized head.
        pub fn set_finalized_head(&mut self, finalized_head: L2BlockInfo) {
            self.sync_state = self.sync_state.apply_update(EngineSyncStateUpdate {
                finalized_head: Some(finalized_head),
                ..Default::default()
            });
        }
    }

    #[rstest]
    #[case::set_unsafe(EngineState::set_unsafe_head, Metrics::UNSAFE_BLOCK_LABEL, 1)]
    #[case::set_safe_head(EngineState::set_safe_head, Metrics::SAFE_BLOCK_LABEL, 2)]
    #[case::set_finalized_head(EngineState::set_finalized_head, Metrics::FINALIZED_BLOCK_LABEL, 3)]
    #[cfg(feature = "metrics")]
    fn test_chain_label_metrics(
        #[case] set_fn: impl Fn(&mut EngineState, L2BlockInfo),
        #[case] label_name: &str,
        #[case] number: u64,
    ) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            let mut state = EngineState::default();
            set_fn(
                &mut state,
                L2BlockInfo {
                    block_info: BlockInfo { number, ..Default::default() },
                    ..Default::default()
                },
            );
        });

        assert!(handle.render().contains(
            format!("base_node_block_labels{{label=\"{label_name}\"}} {number}").as_str()
        ));
    }

    #[rstest]
    #[case::set_unsafe(EngineState::set_unsafe_head, Metrics::UNSAFE_BLOCK_LABEL)]
    #[case::set_safe_head(EngineState::set_safe_head, Metrics::SAFE_BLOCK_LABEL)]
    #[case::set_finalized_head(EngineState::set_finalized_head, Metrics::FINALIZED_BLOCK_LABEL)]
    #[cfg(feature = "metrics")]
    fn test_chain_refs_latency_metrics(
        #[case] set_fn: impl Fn(&mut EngineState, L2BlockInfo),
        #[case] label_name: &str,
    ) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() - 10;

        metrics::with_local_recorder(&recorder, || {
            let mut state = EngineState::default();
            set_fn(
                &mut state,
                L2BlockInfo {
                    block_info: BlockInfo { timestamp, ..Default::default() },
                    ..Default::default()
                },
            );
        });

        let rendered = handle.render();
        let latency_line = rendered
            .lines()
            .find(|l| {
                l.starts_with(&format!("base_node_block_refs_latency{{label=\"{label_name}\"}}"))
            })
            .expect("latency metric not found");
        let latency: f64 =
            latency_line.split_whitespace().last().unwrap_or("0").parse().unwrap_or(0.0);
        assert!((9.0..30.0).contains(&latency), "latency {latency} not in expected range [9, 30)");
    }
}
