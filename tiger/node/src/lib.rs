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
pub use engine::{EngineState, Version as EngineVersion};

mod node;
pub use node::{Node, RpcError};

mod rpc;

mod http;
pub use http::{JwtSecret, serve};

#[cfg(feature = "edge-coverage")]
mod edges;
#[cfg(feature = "edge-coverage")]
pub use edges::EdgeCounters;
