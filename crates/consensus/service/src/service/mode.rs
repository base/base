//! Contains enums that configure the mode for the node to operate in.

use std::num::NonZeroU64;

/// CLI-facing node mode. Variants carry no associated data; use
/// [`NodeMode::try_into_operating_mode`] to attach runtime configuration and obtain a
/// [`NodeOperatingMode`].
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
    /// Isolated sequencer: produces private blocks without canonical-chain ingress or payload
    /// publication.
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

    /// Converts this CLI mode into a [`NodeOperatingMode`], attaching the shadow-cycle block count
    /// when the mode is [`Self::ShadowSequencer`].
    ///
    /// # Panics
    ///
    /// Panics if `self` is [`Self::ShadowSequencer`] and `shadow_blocks_per_cycle` is [`None`].
    /// The CLI validates that `--sequencer.shadow-blocks-per-cycle` is provided before this is
    /// called.
    pub const fn try_into_operating_mode(
        self,
        shadow_blocks_per_cycle: Option<NonZeroU64>,
    ) -> Result<NodeOperatingMode, &'static str> {
        match self {
            Self::Validator => Ok(NodeOperatingMode::Validator),
            Self::Sequencer => match shadow_blocks_per_cycle {
                None => Ok(NodeOperatingMode::Sequencer),
                Some(_) => {
                    Err("--sequencer.shadow-blocks-per-cycle requires --mode ShadowSequencer")
                }
            },
            Self::ShadowSequencer => match shadow_blocks_per_cycle {
                Some(blocks_per_cycle) => {
                    Ok(NodeOperatingMode::ShadowSequencer { blocks_per_cycle })
                }
                None => Err("--mode ShadowSequencer requires --sequencer.shadow-blocks-per-cycle"),
            },
            Self::IsolatedSequencer => Ok(NodeOperatingMode::IsolatedSequencer),
        }
    }
}

/// Runtime node operating mode with all configuration attached.
///
/// Constructed from a [`NodeMode`] via [`NodeMode::into_operating_mode`] after CLI argument
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
    /// Isolated sequencer: produces private blocks without canonical-chain ingress or payload
    /// publication.
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

    /// Returns whether derivation and network actors should be constructed.
    pub const fn derivation_enabled(&self) -> bool {
        !self.is_isolated()
    }
}
