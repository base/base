#![doc = include_str!("../README.md")]

mod protocol;
pub use protocol::{
    COVERAGE_ENV, ConfigLogOutcome, ConfigLogResult, HeaderResults, L1Receipt, Log, Mode,
    PostAccount, Receipt, Rejected, Request, RequestHeader, Response, SystemConfigInput,
    SystemConfigOutput, SystemConfigValues, Witness,
};

mod runner;
pub use runner::{Adapter, AdapterSpec, CallFailure};

mod compare;
pub use compare::{CANONICAL, Difference, Verdict};

mod campaign;
pub use campaign::{Campaign, Summary};

mod fuzz;
pub use fuzz::{
    AdapterResults, ByteEdit, ChainDriver, ChainEnd, ChainOutcome, ChainRecord, ChainSummary, CodeEdit, Corpus,
    CorpusCase, CorpusEntry, CoverageReport, FeeTarget, Finding, FindingGroup, FindingSignature,
    FuzzCase, FuzzConfig, FuzzState, FuzzStats, Fuzzer, GasTarget, L1Tweak, Minimized, Minimizer,
    Mutation, MutationStats, Mutator, Sample, Slot, SlotKind, Tweak,
};

mod mainnet;
pub use mainnet::{
    BlockPool, BlockResult, CallEdit, EraTally, HeaderTweak, MainCase, MainFindingGroup,
    MainFuzzConfig, MainFuzzStats, MainFuzzer, MainnetFetcher, MainnetReplay, ProofWitness,
    RealCall, RealCase, RealItem, RealState, RpcClient, TX_GAS_LIMIT_CAP, Touched, Transplant,
    TransplantItem,
};

mod r#gen;
pub use r#gen::{
    AbiMutation, AbiType, Account, Asm, BlockCtx, Blocks, Bootstrap, Bootstrapped, CHAIN, COLLIDER,
    COLLIDER_NONCES, CREATOR, CREATOR_MAX_NONCE, Caller, ChainGenerator, ChainParam, ConfigLogs,
    ConfigUpdate, DESTRUCTOR_TO, DeployedProgram, Ending, Era, FILLER, Fragment, Generator,
    GeneratorOptions, Genesis, Keys, L1Origin, Native, NativeAbi, NativeCall, NativeTarget, Op,
    Param, PendingDeploy, PlannedTx, Planner, Policy, PrecompileCall, Precompiles, Program,
    ProgramCtx, RECURSER, REFUNDER, RELAY, Registry, Rng, STRESSOR, Scenarios, Schedule, Shape,
    Stress, StressPlan, SysConfig, Template, TemplateInput, Templates, Token, TrafficMix,
    TrieBuilder, TxSpec, Txs, WorldState, era_blocks,
};

mod nodes;
pub use nodes::{
    ChainBlock, ChainFollower, ChainIndex, ChainTree, ChainTx, CoverageReading, Divergence,
    FAMILIES, GuideReport, GuideSample, GuideState, Lag, MethodStats, NodeClient, NodeGenesis,
    NodeProcess, PathStep, Reply, RpcCampaign, RpcCase, RpcCompare, RpcConfig, RpcDifference,
    RpcExclude, RpcFinding, RpcGen, RpcGuide, RpcMutator, RpcSummary, RpcVerdict, StateCampaign,
    StateConfig, StateFinding, StateInput, StateMutator, StateOp, StateSession, StateSummary,
    TreeBlock,
};
