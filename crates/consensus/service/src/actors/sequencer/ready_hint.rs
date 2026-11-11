//! Outcomes of frozen-payload hints that did not trigger a seal.

/// Why a frozen-payload hint from the execution layer did not trigger a seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyHintDropReason {
    /// The seal deadline fired first: a payload is moving through the seal pipeline, or the
    /// hint names the payload that was sealed last.
    AlreadySealing,
    /// The sequencer is stopped.
    Inactive,
    /// No built payload is waiting to be sealed.
    NoBuild,
    /// The hint names a different payload than the one waiting to be sealed.
    PayloadIdMismatch,
    /// The hint was built on a different parent, as after an engine reset.
    ParentMismatch,
    /// The waiting payload precedes Denim, where blocks seal on the deadline only.
    PreDenim,
    /// The hint channel overflowed before the sequencer read it.
    Lagged,
}

impl ReadyHintDropReason {
    /// Returns the bounded metrics label for this drop reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlreadySealing => "already_sealing",
            Self::Inactive => "inactive",
            Self::NoBuild => "no_build",
            Self::PayloadIdMismatch => "payload_id_mismatch",
            Self::ParentMismatch => "parent_mismatch",
            Self::PreDenim => "pre_denim",
            Self::Lagged => "lagged",
        }
    }
}
