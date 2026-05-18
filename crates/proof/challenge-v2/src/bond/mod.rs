//! Bond pipeline: discovery of games whose bond pays our claim
//! addresses, then per-game workers that resolve, unlock, withdraw,
//! and close each game.

mod discovery;
pub use discovery::{BondCandidate, BondDiscovery};

mod action;
pub use action::{BondAction, BondRequest};

mod delayed_weth_resolver;
pub use delayed_weth_resolver::{DelayedWETHResolver, L1DelayedWETHResolver};

mod worker;
pub use worker::{BondError, BondWorkerDeps, run_bond_worker};

mod pool;
pub use pool::BondPool;
