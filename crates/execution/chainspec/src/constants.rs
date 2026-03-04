//! OP stack variation of chain spec constants.

//------------------------------- BASE MAINNET -------------------------------//

/// Max gas limit on Base: <https://basescan.org/block/17208876>
pub const BASE_MAINNET_MAX_GAS_LIMIT: u64 = 105_000_000;

//------------------------------- BASE SEPOLIA -------------------------------//

/// Max gas limit on Base Sepolia: <https://sepolia.basescan.org/block/12506483>
pub const BASE_SEPOLIA_MAX_GAS_LIMIT: u64 = 45_000_000;

//------------------------ BASE DEVNET-0-SEPOLIA-DEV-0 ------------------------//

/// Gas limit on Base devnet-0-sepolia-dev-0 at genesis.
pub const BASE_DEVNET_0_SEPOLIA_DEV_0_GAS_LIMIT: u64 = 25_000_000;
