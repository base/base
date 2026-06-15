#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

mod account_config;
pub use account_config::{AccountConfigurationStorage, AccountState, ActorConfig, LockStatus};
