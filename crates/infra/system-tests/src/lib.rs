#![doc = include_str!("../README.md")]

mod client;
pub use client::TipsRpcClient;

mod fixtures;
pub use fixtures::{
    create_funded_signer, create_load_test_transaction, create_optimism_provider,
    create_signed_transaction, create_test_signer,
};

mod load_test;
pub use load_test::{config, load, setup};
