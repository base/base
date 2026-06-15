#![doc = include_str!("../README.md")]

mod error;
pub use error::AuthError;

mod outcome;
pub use outcome::DispatchOutcome;

mod dispatch;
pub use dispatch::AuthenticatorDispatch;
