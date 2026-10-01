use clap::ValueEnum;

/// Mode for skipping validity transactions that rest at the canonical head.
///
/// See [`RestingPredicates`](crate::RestingPredicates) for when a transaction rests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RestingPredicateMode {
    /// Every validity transaction is evaluated in every flashblock.
    #[default]
    Off,
    /// Resting transactions are classified and still evaluated. A resting transaction whose
    /// predicates match is counted as a mismatch, which would be a missed inclusion under
    /// enforcement.
    Shadow,
    /// Resting transactions are parked under their blocking predicate without evaluation.
    Enforce,
}

impl RestingPredicateMode {
    /// Returns whether resting transactions are classified.
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Shadow | Self::Enforce)
    }

    /// Returns whether resting transactions skip evaluation.
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforce)
    }
}
