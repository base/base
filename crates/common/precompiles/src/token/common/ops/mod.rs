//! Capability extension traits for B-20 token variants.
//!
//! Each trait provides a composable set of token operations with default implementations
//! built entirely on top of [`ITokenCoreAccounting`]. A token variant opts in to a
//! capability by implementing the corresponding trait — no body required when the default
//! impl is sufficient.
//!
//! [`ITokenCoreAccounting`]: crate::token::common::ITokenCoreAccounting

mod burnable;
mod mintable;
mod pausable;
mod permittable;
mod redeemable;
mod token_admin;
mod transferable;

pub use burnable::Burnable;
pub use mintable::Mintable;
pub use pausable::Pausable;
pub use permittable::Permittable;
pub use redeemable::Redeemable;
pub use token_admin::TokenAdmin;
pub use transferable::Transferable;
