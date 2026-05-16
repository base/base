//! Shared business logic for all Base-native token variants.

mod token;
mod token_accounting;
pub mod ops;

pub use token::Token;
pub use token_accounting::TokenAccounting;
pub use ops::{Burnable, Mintable, Pausable, Permittable, Redeemable, Configurable, Transferable};

use alloy_primitives::U256;

/// Capability bit: `pause` / `unpause` are enabled on this token.
pub const CAPABILITY_PAUSABLE: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Capability bit: `setSupplyCap` is enabled on this token.
pub const CAPABILITY_CAP_MUTABLE: U256 = U256::from_limbs([2, 0, 0, 0]);
