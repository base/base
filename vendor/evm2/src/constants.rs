//! EVM constants.

use alloy_primitives::{B256, b256};

/// Maximum deployed contract bytecode size.
///
/// EIP-170 - Contract code size limit.
pub const MAX_CODE_SIZE: usize = 0x6000;

/// Maximum contract creation initcode size.
///
/// EIP-3860 - Limit and meter initcode.
pub const MAX_INITCODE_SIZE: usize = 2 * MAX_CODE_SIZE;

/// Maximum deployed contract bytecode size since Amsterdam.
///
/// EIP-7954 raises the EIP-170 limit to 0x10000 (64 KiB); initcode stays at twice
/// the code size (execution-specs `MAX_CODE_SIZE` / `MAX_INIT_CODE_SIZE`).
pub const MAX_CODE_SIZE_AMSTERDAM: usize = 0x10000;

/// Maximum contract creation initcode size since Amsterdam.
pub const MAX_INITCODE_SIZE_AMSTERDAM: usize = 2 * MAX_CODE_SIZE_AMSTERDAM;

/// Cancun blob base fee update fraction.
pub const BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN: u64 = 3_338_477;

/// Prague blob base fee update fraction.
pub const BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE: u64 = 5_007_716;

/// Amsterdam blob base fee update fraction (BPO2 blob schedule).
pub const BLOB_BASE_FEE_UPDATE_FRACTION_AMSTERDAM: u64 = 11_684_671;

/// Maximum message call depth.
pub const CALL_DEPTH_LIMIT: u16 = 1024;

/// Maximum EVM stack height.
pub const STACK_LIMIT: usize = 1024;

/// Number of recent block hashes available to the `BLOCKHASH` opcode.
pub const BLOCK_HASH_HISTORY: u64 = 256;

/// EIP-7702 version magic.
pub const EIP7702_MAGIC: u16 = 0xEF01;
/// EIP-7702 version magic bytes.
pub const EIP7702_MAGIC_BYTES: &[u8] = &EIP7702_MAGIC.to_be_bytes();
/// EIP-7702 version.
pub const EIP7702_VERSION: u8 = 0;
/// EIP-7702 bytecode length.
///
/// 2 (magic) + 1 (version) + 20 (address) = 23 bytes.
pub const EIP7702_BYTECODE_LEN: usize = 23;

/// EIP-7708 ETH transfer log topic.
pub const EIP7708_TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
