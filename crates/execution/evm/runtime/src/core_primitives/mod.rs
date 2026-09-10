//! Base EVM constants, fork identifiers, bytecode, and opcodes.

pub mod constants;
pub mod eip170;
pub mod eip2780;
pub mod eip3860;
pub mod eip4844;
pub mod eip7702;
pub mod eip7708;
pub mod eip7823;
pub mod eip7825;
pub mod eip7907;
pub mod eip7954;
pub mod eip8037;
pub mod eip8038;
pub mod hardfork;
pub mod hints_util;
mod once_lock;

// Reexport alloy primitives.
pub use alloy_primitives::{
    self, Address, B256, Bytes, FixedBytes, I128, I256, Log, LogData, TxKind, U128, U256, address,
    b256, bytes, fixed_bytes, hex, hex_literal, keccak256,
    map::{
        self, AddressIndexMap, AddressMap, AddressSet, B256Map, HashMap, HashSet, IndexMap,
        U256Map, hash_map, hash_set, indexmap,
    },
    ruint, uint,
};
pub use constants::*;
pub use once_lock::OnceLock;

mod values;
pub use values::{
    ONE_ETHER, ONE_GWEI, SHORT_ADDRESS_CAP, StorageKey, StorageKeyMap, StorageValue, short_address,
};

mod bytecode;
pub use bytecode::{Bytecode, BytecodeKind};

mod decode_errors;
pub use decode_errors::BytecodeDecodeError;

mod iter;
pub use iter::BytecodeIterator;

mod legacy;
pub use legacy::JumpTable;

/// Opcode identifiers, metadata, and optional text parsing.
pub mod opcode;
pub use opcode::OpCode;

/// Bytecode utility operations.
pub mod utils;
pub use bitvec;
