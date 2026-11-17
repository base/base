//! Coverage-guided differential fuzzing: case descriptors the generator rebuilds into
//! well-formed requests, structure-aware mutations, a corpus scheduled by feature rarity, finding
//! minimization, and a multi-block chain driver.

mod case;
pub use case::{
    ByteEdit, CodeEdit, FeeTarget, FuzzCase, GasTarget, L1Tweak, Slot, SlotKind, Tweak,
};

mod mutate;
pub use mutate::{Mutation, Mutator};

mod corpus;
pub use corpus::{Corpus, CorpusEntry};

mod minimize;
pub use minimize::{AdapterResults, FindingSignature, Minimized, Minimizer};

mod engine;
pub use engine::{
    CoverageReport, Finding, FindingGroup, FuzzConfig, FuzzState, FuzzStats, Fuzzer, MutationStats,
    Sample,
};

mod chain;
pub use chain::{ChainDriver, ChainEnd, ChainOutcome, ChainSummary};
