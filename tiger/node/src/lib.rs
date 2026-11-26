#![doc = include_str!("../README.md")]

mod adapter;
pub use adapter::Adapter;

mod witness;
pub use witness::{
    GenesisAccount, NodeSource, WitnessDb, WitnessError, WitnessNodes, WitnessState,
    build_state, storage_root,
};

mod exec;
pub use exec::{BlockExecution, Built, Executed, Validated};

mod genesis;
pub use genesis::{Genesis, genesis_changes};

mod bench;
pub use bench::run as bench;

mod exec_cache;
pub use exec_cache::ExecCache;

mod state;
pub use state::{Misses, NodeDb, NodeState, ParentState, StateReadError, TransactionChanges, block_changes};

mod chain;
pub use chain::{
    BLOCK_HASH_WINDOW, Chain, Forkchoice, FoundBlock, MEMORY_BLOCK_BUFFER_TARGET, MemRebuild, PERSISTENCE_BACKPRESSURE_THRESHOLD, PERSISTENCE_THRESHOLD, PersistBatch, StatelessStart,
    StoredBlock, Validation,
};

mod upstream;
pub use upstream::{BlockKeys, ExecutionWitness, Upstream, Witnesses};

mod engine;
pub use engine::{EngineState, Imported, Proofs, Version as EngineVersion};

mod prewarm;
pub use prewarm::Prewarm;

mod persist;
pub use persist::Persistence;

mod sync;
pub use sync::{BACKFILL_THRESHOLD, Sync};

mod node;
pub use node::{Node, RpcError};

mod rpc;
pub use rpc::{RpcConfig, RpcFilters};

mod trie_mode;
pub use trie_mode::handle as handle_trie_request;

mod http;
pub use http::{JwtSecret, serve};

mod log;
pub use log::Log;

#[cfg(feature = "edge-coverage")]
mod edges;
#[cfg(feature = "edge-coverage")]
pub use edges::EdgeCounters;
