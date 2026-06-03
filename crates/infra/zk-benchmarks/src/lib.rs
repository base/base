#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod config;
pub use config::{
    B20SequenceWorkloadConfig, BenchmarkConfig, BenchmarkConfigOverrides, BenchmarkProfile,
    WorkloadConfig,
};

mod profile;
pub use profile::Profiles;

mod proof_config;
pub use proof_config::{ProofConfig, ProofMode};
