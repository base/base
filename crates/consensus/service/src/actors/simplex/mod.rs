//! Simplex consensus actor.
//!
//! A net-new, feature-flagged consensus actor built on commonware simplex
//! (stable-leader), running in-process to replace op-conductor's out-of-process
//! Raft leadership + payload commit on the sequencer hot path. Phase 1 is a no-op
//! skeleton (request/response plumbing + read-side status watch, no consensus
//! logic); the commonware engine and its dedicated p2p transport arrive in Phase
//! 2. See `docs/consensus-simplex/DESIGN.md`.

mod actor;
pub use actor::{SimplexActor, SimplexMode};

mod client;
pub use client::{
    ConsensusProposer, ConsensusStatus, ConsensusStatusReader, SimplexClient, SimplexRequest,
};
#[cfg(test)]
pub use client::{MockConsensusProposer, MockConsensusStatusReader};

mod error;
pub use error::SimplexError;

mod shadow;
pub use shadow::{ComparisonOutcome, ShadowComparator};

#[cfg(feature = "simplex")]
mod consensus;
#[cfg(feature = "simplex")]
pub use consensus::{
    PinnedLeaderConfig, PinnedLeaderElector, SIMPLEX_CERTIFICATE_CHANNEL, SIMPLEX_RESOLVER_CHANNEL,
    SIMPLEX_VOTE_CHANNEL, SimplexActivity, SimplexCertificate, SimplexConfig, SimplexConfigBuilder,
    SimplexDigest, SimplexNetReceiver, SimplexNetSender, SimplexPublicKey, SimplexRuntimeContext,
    SimplexScheme, SimplexStrategy, StatusReporter, StubAutomaton, StubBlocker, StubRelay,
};
