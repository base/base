#![doc = include_str!("../README.md")]

mod protocol;
pub use protocol::{
    COVERAGE_ENV, HeaderResults, Log, Mode, PostAccount, Receipt, Rejected, Request, RequestHeader,
    Response, Witness,
};

mod runner;
pub use runner::{Adapter, AdapterSpec, CallFailure};

mod compare;
pub use compare::{CANONICAL, Difference, Verdict};

mod campaign;
pub use campaign::{Campaign, Summary};

mod fuzz;
pub use fuzz::{
    AdapterResults, ByteEdit, ChainDriver, ChainEnd, ChainOutcome, ChainSummary, CodeEdit, Corpus,
    CorpusEntry, CoverageReport, FeeTarget, Finding, FindingGroup, FindingSignature, FuzzCase,
    FuzzConfig, FuzzState, FuzzStats, Fuzzer, GasTarget, L1Tweak, Minimized, Minimizer, Mutation,
    MutationStats, Mutator, Sample, Slot, SlotKind, Tweak,
};

mod mainnet;
pub use mainnet::{MainnetFetcher, RpcClient};

mod r#gen;
pub use r#gen::{
    AbiMutation, AbiType, Account, Asm, BlockCtx, Blocks, Bootstrap, Bootstrapped, CHAIN, COLLIDER,
    COLLIDER_NONCES, CREATOR, CREATOR_MAX_NONCE, Caller, ChainGenerator, DESTRUCTOR_TO,
    DeployedProgram, Ending, Era, FILLER, Fragment, Generator, GeneratorOptions, Genesis, Keys,
    L1Origin, Native, NativeAbi, NativeCall, NativeTarget, Op, Param, PendingDeploy, PlannedTx,
    Planner, Policy, PrecompileCall, Precompiles, Program, ProgramCtx, RECURSER, REFUNDER, RELAY,
    Registry, Rng, Scenarios, Schedule, Shape, SysConfig, Template, TemplateInput, Templates,
    Token, TrafficMix, TrieBuilder, TxSpec, Txs, WorldState, era_blocks,
};
