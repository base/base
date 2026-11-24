//! Base mainnet case fetcher: turns a mainnet block number into a validate-mode [`Request`]
//! built from the block's raw RLP and its `debug_executionWitness`, with an on-disk cache.
//!
//! [`Request`]: crate::Request

mod rpc;
pub use rpc::RpcClient;

mod fetcher;
pub use fetcher::MainnetFetcher;

mod proofs;
pub use proofs::{ProofWitness, Touched};

mod replay;
pub use replay::{BlockResult, EraTally, MainnetReplay};

mod mutate;
pub use mutate::{
    BlockPool, CallEdit, HeaderTweak, RealCall, RealCase, RealItem, TX_GAS_LIMIT_CAP,
};

mod transplant;
pub use transplant::{RealState, Transplant, TransplantItem};

mod mainfuzz;
pub use mainfuzz::{MainCase, MainFindingGroup, MainFuzzConfig, MainFuzzStats, MainFuzzer};
