#![doc = include_str!("../README.md")]

mod error;
pub use error::AuthorizeError;

mod resolved;
pub use resolved::ResolvedActor;

mod authorize;
pub use authorize::ActorAuthorizer;
