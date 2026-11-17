//! Deterministic generator of synthetic Base mainnet blocks and chains in build mode.

mod rng;
pub use rng::Rng;

mod era;
pub use era::{Era, Schedule};

mod trie;
pub use trie::TrieBuilder;

mod state;
pub use state::{Account, WorldState};

mod keys;
pub use keys::Keys;

mod bootstrap;
pub use bootstrap::{Bootstrap, Bootstrapped};

mod abi;
pub use abi::{AbiMutation, AbiType, NativeAbi, NativeTarget};

mod asm;
pub use asm::{Asm, Op};

mod precompile;
pub use precompile::{PrecompileCall, Precompiles};

mod program;
pub use program::{Ending, Fragment, Param, Program, ProgramCtx, Templates};

mod registry;
pub use registry::{DeployedProgram, PendingDeploy, Policy, Registry, Token};

mod native;
pub use native::{Caller, Native, NativeCall};

mod tx;
pub use tx::{Shape, TxSpec, Txs};

mod block;
pub use block::{Blocks, L1Origin, SysConfig};

mod sysconfig;
pub use sysconfig::{ConfigLogs, ConfigUpdate};

mod plan;
pub use plan::{BlockCtx, PlannedTx, Planner};

mod genesis;
pub use genesis::{
    COLLIDER, COLLIDER_NONCES, CREATOR, CREATOR_MAX_NONCE, DESTRUCTOR_TO, FILLER, Genesis,
    RECURSER, REFUNDER, RELAY,
};

mod template;
pub use template::{CHAIN, Template, TemplateInput, TrafficMix};

mod scenario;
pub use scenario::Scenarios;

mod case;
pub use case::{Generator, GeneratorOptions, era_blocks};

mod chain;
pub use chain::ChainGenerator;
