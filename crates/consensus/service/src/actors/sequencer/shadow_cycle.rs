//! Shadow sequencer build and reconciliation cycle state.

use std::sync::Arc;

use base_protocol::L2BlockInfo;
use tokio::task::JoinHandle;

use crate::{EngineClientError, EngineClientResult, PayloadSealer, SequencerEngineClient};

/// Background reconciliation task for a completed private build cycle.
pub type ShadowReconciliationTask = JoinHandle<EngineClientResult<Option<L2BlockInfo>>>;

/// Progress through one private shadow build and reconciliation cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowCycle {
    /// Privately building blocks before the configured cycle limit.
    Building {
        /// Next private block height expected from the engine.
        next_height: u64,
        /// Number of private blocks inserted in this cycle.
        built: u64,
    },
    /// Waiting for authenticated P2P payloads to replace the private branch.
    AwaitingReconciliation {
        /// Last privately built head that reconciliation must replace.
        shadow_head: L2BlockInfo,
    },
}

impl ShadowCycle {
    /// Starts a private build cycle on the provided canonical head.
    pub fn building(canonical_head: L2BlockInfo) -> Result<Self, EngineClientError> {
        let next_height = canonical_head.block_info.number.checked_add(1).ok_or_else(|| {
            EngineClientError::InvalidShadowReconciliation(
                "private cycle block number overflow".to_string(),
            )
        })?;
        Ok(Self::Building { next_height, built: 0 })
    }

    /// Returns whether no private block has yet been inserted in this cycle.
    pub const fn is_at_start(self) -> bool {
        matches!(self, Self::Building { built: 0, .. })
    }

    /// Returns the reconciliation target when private building has reached its cycle limit.
    pub const fn reconciliation_target(self) -> Option<L2BlockInfo> {
        match self {
            Self::AwaitingReconciliation { shadow_head } => Some(shadow_head),
            Self::Building { .. } => None,
        }
    }

    /// Starts one reconciliation attempt for the current private head.
    pub fn start_reconciliation<E>(
        self,
        engine_client: Arc<E>,
    ) -> Result<ShadowReconciliationTask, EngineClientError>
    where
        E: SequencerEngineClient + 'static,
    {
        let shadow_head = self.reconciliation_target().ok_or_else(|| {
            EngineClientError::InvalidShadowReconciliation(
                "reconciliation attempted before private cycle completed".to_string(),
            )
        })?;
        Ok(tokio::spawn(async move { engine_client.reconcile_shadow(shadow_head).await }))
    }

    /// Validates that the engine acknowledged the exact payload and height expected by this cycle.
    /// A mismatch indicates an unexpected reset or reorg and must terminate shadow sequencing.
    pub fn validate_insertion(
        self,
        sealer: &PayloadSealer,
        inserted_head: L2BlockInfo,
    ) -> Result<(), EngineClientError> {
        let expected_height = match self {
            Self::Building { next_height, .. } => next_height,
            Self::AwaitingReconciliation { .. } => {
                return Err(EngineClientError::InvalidShadowReconciliation(
                    "private insertion attempted while awaiting reconciliation".to_string(),
                ));
            }
        };
        if !sealer.matches_inserted_head(inserted_head)
            || inserted_head.block_info.number != expected_height
        {
            return Err(EngineClientError::InvalidShadowReconciliation(
                "private insertion acknowledgement does not match the cycle".to_string(),
            ));
        }
        Ok(())
    }

    /// Records an acknowledged private insertion and returns whether reconciliation should start.
    pub fn record_insertion(
        &mut self,
        head: L2BlockInfo,
        blocks_per_cycle: u64,
    ) -> Result<bool, EngineClientError> {
        match self {
            Self::Building { next_height, built } => {
                *built += 1;
                if *built == blocks_per_cycle {
                    *self = Self::AwaitingReconciliation { shadow_head: head };
                    Ok(true)
                } else {
                    *next_height = next_height.checked_add(1).ok_or_else(|| {
                        EngineClientError::InvalidShadowReconciliation(
                            "private cycle block number overflow".to_string(),
                        )
                    })?;
                    Ok(false)
                }
            }
            Self::AwaitingReconciliation { .. } => {
                Err(EngineClientError::InvalidShadowReconciliation(
                    "private insertion attempted while awaiting reconciliation".to_string(),
                ))
            }
        }
    }

    /// Starts the next private build cycle on a successfully reconciled canonical head.
    pub fn reconcile(&mut self, head: L2BlockInfo) -> Result<(), EngineClientError> {
        *self = Self::building(head)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::ShadowCycle;

    fn head(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                number,
                hash: B256::with_last_byte(number as u8),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn reaches_reconciliation_after_configured_insertions() {
        let mut cycle = ShadowCycle::building(head(10)).unwrap();
        assert!(cycle.is_at_start());
        assert!(!cycle.record_insertion(head(11), 2).unwrap());
        assert!(!cycle.is_at_start());
        assert!(cycle.record_insertion(head(12), 2).unwrap());
        assert_eq!(cycle.reconciliation_target(), Some(head(12)));
    }

    #[test]
    fn reconciliation_starts_next_build_cycle() {
        let mut cycle = ShadowCycle::AwaitingReconciliation { shadow_head: head(12) };
        cycle.reconcile(head(12)).unwrap();
        assert_eq!(cycle, ShadowCycle::Building { next_height: 13, built: 0 });
        assert!(cycle.is_at_start());
    }
}
