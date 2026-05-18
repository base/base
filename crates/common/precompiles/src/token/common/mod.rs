//! Shared business logic for all Base-native token variants.

mod ops;
mod token;
mod token_accounting;

use alloy_primitives::U256;
pub use ops::{Burnable, Configurable, Mintable, Pausable, Permittable, Redeemable, Transferable};
pub use token::Token;
pub use token_accounting::TokenAccounting;

/// Capability bit: `pause` / `unpause` are enabled on this token.
pub const CAPABILITY_PAUSABLE: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Capability bit: `setSupplyCap` is enabled on this token.
pub const CAPABILITY_CAP_MUTABLE: U256 = U256::from_limbs([2, 0, 0, 0]);
