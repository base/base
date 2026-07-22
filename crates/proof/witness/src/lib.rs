#![doc = include_str!("../README.md")]

mod assembler;
pub use assembler::PreimageMap;

mod error;
pub use error::{Result, WitnessError};

mod generator;
pub use generator::{
    L1RpcBlock, L1RpcTransaction, L2_HEADER_LOOKBACK, L2RpcBlock, WitnessConfig, WitnessGenerator,
    WitnessProviders,
};
