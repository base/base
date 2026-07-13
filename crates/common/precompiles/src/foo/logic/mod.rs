//! Versioned business logic for the `foo` precompile.
//!
//! [`FooLogic`] is the append-only business-logic interface. Each version is a
//! distinct type implementing it; once a version is activated at a hardfork it
//! is frozen, and new behavior is introduced by a new version that composes the
//! previous one — Rust's stand-in for inheritance (see [`FooV2`]).

use alloc::string::String;

use alloy_primitives::Address;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{FooStorage, IFoo};

mod v1;
pub use v1::FooV1;

mod v2;
pub use v2::FooV2;

/// Append-only business-logic interface shared by every `foo` version.
pub trait FooLogic {
    /// Returns the greeting for `helloWorld()`.
    ///
    /// Goal 1 (changes to existing logic): the returned value differs between
    /// versions, and each version's value is frozen once activated.
    fn hello_world(&self) -> String;

    /// Handles `greet(name)`, returning a personalized greeting.
    ///
    /// Goal 3 (new methods): `greet` is appended in a later version. The
    /// default implementation reproduces the pre-activation behavior — a revert
    /// with [`IFoo::UnsupportedBeforeActivation`] — so versions that predate the
    /// method stay unchanged.
    fn greet(
        &self,
        _storage: &mut FooStorage<'_>,
        _caller: Address,
        _name: String,
    ) -> Result<String> {
        Err(BasePrecompileError::revert(IFoo::UnsupportedBeforeActivation {}))
    }
}
