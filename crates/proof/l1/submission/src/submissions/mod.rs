//! Proof submission and dispute-game transaction handling.

mod error;
pub use error::ProofSubmissionError;

mod classifier;
pub use classifier::KnownRevert;

mod submission;
pub use submission::{ChallengeProofSubmission, NullifyProofSubmission};

mod submitter;
pub use submitter::AggregateProofSubmitter;
