//! Capability extension traits for B-20 token variants.
//!
//! Each trait provides a composable set of token operations with default implementations
//! built entirely on top of [`TokenAccounting`]. A token variant opts in to a
//! capability by implementing the corresponding trait — no body required when the default
//! impl is sufficient.
//!
//! [`TokenAccounting`]: crate::token::common::TokenAccounting

mod burnable;
mod configurable;
mod mintable;
mod pausable;
mod permittable;
mod redeemable;
mod transferable;

pub use burnable::Burnable;
pub use configurable::Configurable;
pub use mintable::Mintable;
pub use pausable::Pausable;
pub use permittable::Permittable;
pub use redeemable::Redeemable;
pub use transferable::Transferable;
