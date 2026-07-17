#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod types;
pub use types::{ZkBenchConfig, ZkBenchProofOutcome, ZkBenchSummary, ZkBenchTarget};

mod runner;
pub use runner::ZkBenchRunner;
