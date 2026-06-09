//! Base API types.

mod builder;
pub use builder::Builder;

mod default_ctx;
pub use default_ctx::{BaseContext, DefaultBase};

mod exec;
pub use exec::{BaseContextTr, BaseError};
