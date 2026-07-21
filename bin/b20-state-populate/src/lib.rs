#![doc = include_str!("../README.md")]

mod cli;
pub use cli::{Args, PopulateArgs, SubCommand, VerifyArgs};

mod storage;
pub use storage::{
    B20_ASSET_ROOT, B20_BALANCE_MAPPING_SLOT, B20_CORE_ROOT, B20_DECIMALS_SLOT,
    B20_INITIALIZED_SLOT, B20_MULTIPLIER_SLOT, B20_SUPPLY_CAP_SLOT, B20_TOTAL_SUPPLY_SLOT,
    EVM_TOKEN_ADDRESS, MOCK_B20_ASSET_BYTECODE, address_for_index, b20_balance_slot,
    derive_b20_asset_address, derive_sender_addresses, evm_erc20_balance_slot,
};

mod populate;
pub use populate::Populator;

mod verify;
pub use verify::Verifier;

mod trie_version;
pub use trie_version::StorageTrieVersion;
