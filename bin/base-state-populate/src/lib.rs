#![doc = include_str!("../README.md")]

mod cli;
pub use cli::{Args, PopulateArgs, SubCommand, VerifyArgs};

mod storage;
pub use storage::{address_for_index, derive_sender_addresses, erc20_balance_slot};

mod populate;
pub use populate::Populator;

mod verify;
pub use verify::Verifier;
