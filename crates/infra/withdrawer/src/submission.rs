use alloy_primitives::B256;
use alloy_provider::RootProvider;
use base_tx_manager::SimpleTxManager;
use thiserror::Error;

use crate::{BroadcastJournal, WithdrawerError};

/// Serialized withdrawal sender with its transport's broadcast journal.
#[derive(Debug)]
pub struct WithdrawalSender {
    /// Transaction manager using pending nonces.
    pub manager: SimpleTxManager<RootProvider>,
    /// Hashes recorded by this manager's submission transport.
    /// [`Withdrawer::submit`](crate::Withdrawer::submit) clears it before each send.
    /// Callers should only read it.
    pub journal: BroadcastJournal,
}

/// Submission failure classified by whether any transaction may have reached L1.
#[derive(Debug, Error)]
pub enum SubmitError {
    /// No transaction was broadcast by this submission.
    #[error(transparent)]
    NotSent(WithdrawerError),
    /// At least one transaction may already be onchain; do not infer success from portal state.
    #[error(
        "transactions {transaction_hashes:?} may have been broadcast or included; check them before retrying: {source}"
    )]
    MaybeIncluded {
        /// Every distinct broadcast hash, including fee replacements.
        transaction_hashes: Vec<B256>,
        /// Redacted submission, receipt, event validation, or deadline failure.
        #[source]
        source: Box<WithdrawerError>,
    },
}

impl From<SubmitError> for WithdrawerError {
    fn from(error: SubmitError) -> Self {
        match error {
            SubmitError::NotSent(source) => source,
            error @ SubmitError::MaybeIncluded { .. } => Self::Submit(Box::new(error)),
        }
    }
}
