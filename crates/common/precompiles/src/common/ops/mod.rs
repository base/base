//! Capability extension traits for B-20 token variants.
//!
//! Each trait provides a composable set of token operations with default implementations
//! built entirely on top of [`TokenAccounting`]. A token variant opts in to a
//! capability by implementing the corresponding trait — no body required when the default
//! impl is sufficient.
//!
//! [`TokenAccounting`]: crate::TokenAccounting

mod burnable;
pub use burnable::Burnable;

mod configurable;
pub use configurable::Configurable;

mod guards;
pub use guards::B20Guards;

mod mintable;
pub use mintable::Mintable;

mod pausable;
pub use pausable::Pausable;

mod permittable;
pub use permittable::Permittable;

mod transferable;
pub use transferable::Transferable;
