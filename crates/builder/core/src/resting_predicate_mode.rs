use clap::ValueEnum;

/// Mode for holding back validity transactions that rest under an unchanged predicate.
///
/// See [`RestingPayloadTransactions`](crate::RestingPayloadTransactions) for when a transaction
/// rests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RestingPredicateMode {
    /// Resting transactions are not tracked.
    #[default]
    Off,
    /// Resting transactions are tracked and still evaluated. A resting transaction whose
    /// predicates match is counted as a mismatch, which would be a missed inclusion under
    /// enforcement.
    Shadow,
    /// Resting transactions are parked in the transaction iterator without reaching the build
    /// loop until a commit changes the state their predicate reads.
    Enforce,
}

impl RestingPredicateMode {
    /// Returns whether resting transactions are tracked.
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Shadow | Self::Enforce)
    }

    /// Returns whether resting transactions are held back.
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforce)
    }
}
