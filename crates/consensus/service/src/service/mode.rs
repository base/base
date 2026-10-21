//! Contains enums that configure the mode for the node to operate in.

use std::num::NonZeroU64;

/// The [`NodeMode`] enum represents the CLI-facing modes of operation for the [`RollupNode`].
///
/// Variants carry no associated data. Use [`NodeMode::try_into_operating_mode`] to attach runtime
/// configuration and obtain a [`NodeOperatingMode`].
///
/// [`RollupNode`]: crate::RollupNode
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    derive_more::Display,
    derive_more::FromStr,
    strum::EnumIter,
)]
pub enum NodeMode {
    /// Validator mode.
    #[display("Validator")]
    #[default]
    Validator,
    /// Active sequencer: produces and publishes canonical unsafe blocks.
    #[display("Sequencer")]
    Sequencer,
    /// Shadow sequencer: produces private blocks before reconciling with the canonical chain.
    ///
    /// Requires `--sequencer.shadow-blocks-per-cycle`.
    #[display("ShadowSequencer")]
    ShadowSequencer,
    /// Isolated sequencer: produces private blocks without payload publication, following
    /// canonical gossip only until it reaches the canonical tip at startup.
    #[display("IsolatedSequencer")]
    IsolatedSequencer,
}

impl NodeMode {
    /// Returns `true` if this is [`Self::Validator`].
    pub const fn is_validator(&self) -> bool {
        matches!(self, Self::Validator)
    }

    /// Returns `true` if this is any sequencer variant.
    pub const fn is_sequencer(&self) -> bool {
        !self.is_validator()
    }

    /// Converts this CLI mode and the sequencer flags into a [`NodeOperatingMode`].
    ///
    /// With [`Self::Sequencer`], the `isolated` and `shadow_blocks_per_cycle` flags still select
    /// an isolated or shadow sequencer, so existing deployments and the unified `base sequencer`
    /// command (which fixes the mode to [`Self::Sequencer`]) keep working. Validators ignore both
    /// flags, as before.
    ///
    /// Returns an error if both flags are set, if [`Self::ShadowSequencer`] has no
    /// `shadow_blocks_per_cycle`, or if [`Self::IsolatedSequencer`] has one.
    pub const fn try_into_operating_mode(
        self,
        isolated: bool,
        shadow_blocks_per_cycle: Option<NonZeroU64>,
    ) -> Result<NodeOperatingMode, &'static str> {
        match (self, isolated, shadow_blocks_per_cycle) {
            (Self::Validator, _, _) => Ok(NodeOperatingMode::Validator),
            (_, true, Some(_)) => {
                Err("--sequencer.isolated conflicts with --sequencer.shadow-blocks-per-cycle")
            }
            (Self::Sequencer, false, None) => Ok(NodeOperatingMode::Sequencer),
            (Self::Sequencer, true, None) | (Self::IsolatedSequencer, _, None) => {
                Ok(NodeOperatingMode::IsolatedSequencer)
            }
            (Self::Sequencer | Self::ShadowSequencer, false, Some(blocks_per_cycle)) => {
                Ok(NodeOperatingMode::ShadowSequencer { blocks_per_cycle })
            }
            (Self::ShadowSequencer, _, None) => {
                Err("--mode ShadowSequencer requires --sequencer.shadow-blocks-per-cycle")
            }
            (Self::IsolatedSequencer, false, Some(_)) => {
                Err("--mode IsolatedSequencer conflicts with --sequencer.shadow-blocks-per-cycle")
            }
        }
    }
}

/// Runtime node operating mode with all configuration attached.
///
/// Constructed from a [`NodeMode`] via [`NodeMode::try_into_operating_mode`] after CLI argument
/// resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeOperatingMode {
    /// Validator mode.
    Validator,
    /// Active sequencer: produces and publishes canonical unsafe blocks.
    Sequencer,
    /// Shadow sequencer: produces private blocks before reconciling with the canonical chain.
    ShadowSequencer {
        /// Number of private blocks to build per cycle before reconciling.
        blocks_per_cycle: NonZeroU64,
    },
    /// Isolated sequencer: produces private blocks without payload publication.
    IsolatedSequencer,
}

impl NodeOperatingMode {
    /// Returns `true` if this is [`Self::Validator`].
    pub const fn is_validator(&self) -> bool {
        matches!(self, Self::Validator)
    }

    /// Returns `true` if this is any sequencer variant.
    pub const fn is_sequencer(&self) -> bool {
        !self.is_validator()
    }

    /// Returns `true` if this is [`Self::IsolatedSequencer`].
    pub const fn is_isolated(&self) -> bool {
        matches!(self, Self::IsolatedSequencer)
    }

    /// Returns `true` if this is [`Self::ShadowSequencer`].
    pub const fn is_shadow_sequencer(&self) -> bool {
        matches!(self, Self::ShadowSequencer { .. })
    }

    /// Returns the shadow-cycle block count, or [`None`] when not in shadow mode.
    pub const fn shadow_blocks_per_cycle(&self) -> Option<NonZeroU64> {
        match self {
            Self::ShadowSequencer { blocks_per_cycle } => Some(*blocks_per_cycle),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use rstest::rstest;

    use super::{NodeMode, NodeOperatingMode};

    const CYCLE: Option<NonZeroU64> = NonZeroU64::new(5);

    #[rstest]
    #[case::validator(NodeMode::Validator, false, None, NodeOperatingMode::Validator)]
    #[case::validator_ignores_flags(NodeMode::Validator, true, CYCLE, NodeOperatingMode::Validator)]
    #[case::sequencer(NodeMode::Sequencer, false, None, NodeOperatingMode::Sequencer)]
    #[case::legacy_isolated(NodeMode::Sequencer, true, None, NodeOperatingMode::IsolatedSequencer)]
    #[case::legacy_shadow(
        NodeMode::Sequencer,
        false,
        CYCLE,
        NodeOperatingMode::ShadowSequencer { blocks_per_cycle: NonZeroU64::new(5).unwrap() }
    )]
    #[case::isolated(
        NodeMode::IsolatedSequencer,
        false,
        None,
        NodeOperatingMode::IsolatedSequencer
    )]
    #[case::isolated_with_flag(
        NodeMode::IsolatedSequencer,
        true,
        None,
        NodeOperatingMode::IsolatedSequencer
    )]
    #[case::shadow(
        NodeMode::ShadowSequencer,
        false,
        CYCLE,
        NodeOperatingMode::ShadowSequencer { blocks_per_cycle: NonZeroU64::new(5).unwrap() }
    )]
    fn resolves_operating_mode(
        #[case] mode: NodeMode,
        #[case] isolated: bool,
        #[case] shadow_blocks_per_cycle: Option<NonZeroU64>,
        #[case] expected: NodeOperatingMode,
    ) {
        assert_eq!(mode.try_into_operating_mode(isolated, shadow_blocks_per_cycle), Ok(expected));
    }

    #[rstest]
    #[case::isolated_and_shadow_flags(NodeMode::Sequencer, true, CYCLE)]
    #[case::shadow_without_cycle(NodeMode::ShadowSequencer, false, None)]
    #[case::shadow_with_isolated_flag(NodeMode::ShadowSequencer, true, CYCLE)]
    #[case::isolated_with_cycle(NodeMode::IsolatedSequencer, false, CYCLE)]
    fn rejects_conflicting_flags(
        #[case] mode: NodeMode,
        #[case] isolated: bool,
        #[case] shadow_blocks_per_cycle: Option<NonZeroU64>,
    ) {
        assert!(mode.try_into_operating_mode(isolated, shadow_blocks_per_cycle).is_err());
    }
}
