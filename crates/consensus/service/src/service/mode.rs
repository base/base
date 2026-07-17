//! Contains enums that configure the mode for the node to operate in.

/// The [`NodeMode`] enum represents the modes of operation for the [`RollupNode`].
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
    /// Sequencer mode.
    #[display("Sequencer")]
    Sequencer,
    /// Shadow-drive mode.
    #[display("ShadowDrive")]
    ShadowDrive,
}

impl NodeMode {
    /// Returns `true` if [`Self`] is [`Self::Validator`].
    pub const fn is_validator(&self) -> bool {
        matches!(self, Self::Validator)
    }

    /// Returns `true` if [`Self`] is [`Self::Sequencer`].
    pub const fn is_sequencer(&self) -> bool {
        matches!(self, Self::Sequencer)
    }

    /// Returns `true` if [`Self`] is [`Self::ShadowDrive`].
    pub const fn is_shadow_drive(&self) -> bool {
        matches!(self, Self::ShadowDrive)
    }
}
