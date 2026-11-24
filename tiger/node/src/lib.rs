#![doc = include_str!("../README.md")]

mod adapter;
pub use adapter::Adapter;

mod witness;
pub use witness::{
    GenesisAccount, NodeSource, StateUpdate, WitnessDb, WitnessError, WitnessNodes, WitnessState,
    build_state, storage_root,
};

mod exec;
pub use exec::{BlockExecution, Built, Executed, Validated};

mod genesis;
pub use genesis::{Genesis, genesis_codes};

mod chain;
pub use chain::{Chain, Forkchoice, StoredBlock};

mod engine;
pub use engine::{EngineState, Imported, Version as EngineVersion};

mod sync;
pub use sync::{BACKFILL_THRESHOLD, Sync};

mod node;
pub use node::{Node, RpcError};

mod rpc;

mod trie_mode;
pub use trie_mode::handle as handle_trie_request;

mod http;
pub use http::{JwtSecret, serve};

#[cfg(feature = "edge-coverage")]
mod edges;
#[cfg(feature = "edge-coverage")]
pub use edges::EdgeCounters;
