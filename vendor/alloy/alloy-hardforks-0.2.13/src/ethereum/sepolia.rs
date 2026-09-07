//! Sepolia hardfork starting points

use alloy_primitives::{U256, uint};

/// Paris sepolia hard fork activation block is 1450409.
pub const SEPOLIA_PARIS_BLOCK: u64 = 1_450_409;
/// Paris sepolia hard fork activation terminal total difficulty is 17_000_000_000_000_000_U256.
pub const SEPOLIA_PARIS_TTD: U256 = uint!(17_000_000_000_000_000_U256);
/// Paris sepolia fork block is 1735371. See [`ForkCondition::TTD`](crate::ForkCondition::TTD).
pub const SEPOLIA_PARIS_FORK_BLOCK: u64 = 1_735_371;
/// Shanghai sepolia hard fork activation block is 2990908.
pub const SEPOLIA_SHANGHAI_BLOCK: u64 = 2_990_908;
/// Cancun sepolia hard fork activation block is 5187023.
pub const SEPOLIA_CANCUN_BLOCK: u64 = 5_187_023;
/// Prague sepolia hard fork activation block is 7836331.
pub const SEPOLIA_PRAGUE_BLOCK: u64 = 7_836_331;

/// Paris sepolia hard fork activation timestamp is 1633267481.
pub const SEPOLIA_PARIS_TIMESTAMP: u64 = 1_633_267_481;
/// Prague sepolia hard fork activation block is 1677557088.
pub const SEPOLIA_SHANGHAI_TIMESTAMP: u64 = 1_677_557_088;
/// Cancun sepolia hard fork activation timestamp is 1706655072.
pub const SEPOLIA_CANCUN_TIMESTAMP: u64 = 1_706_655_072;
/// Prague sepolia hard fork activation timestamp is 1741159776.
pub const SEPOLIA_PRAGUE_TIMESTAMP: u64 = 1_741_159_776;
