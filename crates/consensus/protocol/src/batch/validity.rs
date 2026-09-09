//! Contains the [`BatchValidity`], [`BatchDropReason`] and their encodings.

/// Reasons why a batch may be dropped.
///
/// This enum provides detailed context for why a batch was deemed invalid,
/// enabling more precise error handling and testing without relying on log message parsing.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDropReason {
    // === Timestamp-related drops ===
    /// Batch timestamp is later than the next expected L2 block.
    FutureTimestamp,

    // === Parent/origin validation drops ===
    /// Parent hash does not match the L2 safe head.
    ParentHashMismatch,
    /// Batch was included too late (outside sequencer window).
    IncludedTooLate,
    /// Batch epoch is older than the current epoch.
    EpochTooOld,
    /// Batch epoch is too far in the future.
    EpochTooFarInFuture,
    /// Batch epoch hash does not match the L1 origin.
    EpochHashMismatch,

    // === Timestamp/origin relationship drops ===
    /// Batch timestamp is before the L1 origin timestamp.
    TimestampBeforeL1Origin,
    /// Sequencer drift overflow (`checked_add` failed).
    SequencerDriftOverflow,
    /// Batch exceeded sequencer time drift with non-empty transactions.
    SequencerDriftExceeded,
    /// Empty batch could have adopted next L1 origin but didn't.
    SequencerDriftNotAdoptedNextOrigin,

    // === Transaction validation drops ===
    /// Batch contains an empty transaction.
    EmptyTransaction,
    /// Batch contains a deposit transaction (not allowed in batch data).
    DepositTransaction,
    /// EIP-8130 transaction included before Zenith activation.
    Eip8130PreZenith,
}

impl core::fmt::Display for BatchDropReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FutureTimestamp => {
                write!(f, "batch timestamp is in the future")
            }
            Self::ParentHashMismatch => write!(f, "parent hash does not match L2 safe head"),
            Self::IncludedTooLate => write!(f, "batch was included too late"),
            Self::EpochTooOld => write!(f, "batch epoch is too old"),
            Self::EpochTooFarInFuture => write!(f, "batch epoch is too far in the future"),
            Self::EpochHashMismatch => write!(f, "batch epoch hash does not match L1 origin"),
            Self::TimestampBeforeL1Origin => {
                write!(f, "batch timestamp is before L1 origin timestamp")
            }
            Self::SequencerDriftOverflow => write!(f, "sequencer drift calculation overflow"),
            Self::SequencerDriftExceeded => write!(f, "batch exceeded sequencer time drift"),
            Self::SequencerDriftNotAdoptedNextOrigin => {
                write!(f, "empty batch could have adopted next L1 origin")
            }
            Self::EmptyTransaction => write!(f, "batch contains empty transaction"),
            Self::DepositTransaction => write!(f, "batch contains deposit transaction"),
            Self::Eip8130PreZenith => write!(f, "EIP-8130 transaction before Zenith activation"),
        }
    }
}

/// Batch Validity
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchValidity {
    /// The batch is invalid now and in the future, unless we reorg, so it can be discarded.
    /// Contains the reason for dropping the batch.
    Drop(BatchDropReason),
    /// The batch is valid and should be processed
    Accept,
    /// We are lacking L1 information until we can proceed batch filtering
    Undecided,
    /// An old batch that can be skipped without flushing its channel.
    Past,
}

impl core::fmt::Display for BatchValidity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Drop(reason) => write!(f, "Drop({reason})"),
            Self::Accept => write!(f, "Accept"),
            Self::Undecided => write!(f, "Undecided"),
            Self::Past => write!(f, "Past"),
        }
    }
}

impl BatchValidity {
    /// Returns whether the batch is accepted.
    pub const fn is_accept(&self) -> bool {
        matches!(self, Self::Accept)
    }

    /// Returns whether the batch is dropped.
    pub const fn is_drop(&self) -> bool {
        matches!(self, Self::Drop(_))
    }

    /// Returns the drop reason if the batch was dropped.
    pub const fn drop_reason(&self) -> Option<BatchDropReason> {
        match self {
            Self::Drop(reason) => Some(*reason),
            _ => None,
        }
    }

    /// Returns whether the batch is outdated.
    pub const fn is_outdated(&self) -> bool {
        matches!(self, Self::Past)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    #[test]
    fn test_batch_validity() {
        assert!(BatchValidity::Accept.is_accept());
        assert!(BatchValidity::Drop(BatchDropReason::ParentHashMismatch).is_drop());
        assert!(BatchValidity::Past.is_outdated());
    }

    #[test]
    fn test_drop_reason() {
        let validity = BatchValidity::Drop(BatchDropReason::EmptyTransaction);
        assert_eq!(validity.drop_reason(), Some(BatchDropReason::EmptyTransaction));
        assert!(BatchValidity::Accept.drop_reason().is_none());
    }

    #[test]
    fn test_batch_drop_reason_display() {
        assert_eq!(
            format!("{}", BatchDropReason::ParentHashMismatch),
            "parent hash does not match L2 safe head"
        );
        assert_eq!(
            format!("{}", BatchDropReason::EmptyTransaction),
            "batch contains empty transaction"
        );
    }

    #[test]
    fn test_batch_validity_display() {
        assert_eq!(
            format!("{}", BatchValidity::Drop(BatchDropReason::ParentHashMismatch)),
            "Drop(parent hash does not match L2 safe head)"
        );
        assert_eq!(format!("{}", BatchValidity::Accept), "Accept");
    }
}
