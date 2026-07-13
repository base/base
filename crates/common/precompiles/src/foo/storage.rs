//! Storage layout for the `foo` reference precompile.

use alloy_primitives::{Address, address};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, Result};

use crate::IFoo;

/// Persistent state for the `foo` precompile.
///
/// State evolves append-only across versions (execution-consensus Goal 2): new
/// fields may be added, but existing fields keep their slot and meaning so
/// historical replay reads the same values.
#[contract(addr = Self::ADDRESS)]
#[namespace("base.foo")]
pub struct FooStorage {
    /// Number of times each caller has invoked `greet`.
    pub greet_count: Mapping<Address, u64>,
}

impl FooStorage<'_> {
    /// `foo` reference precompile address.
    pub const ADDRESS: Address = address!("f00f000000000000000000000000000000000000");

    /// Records a `greet` invocation: bumps the caller's counter and emits
    /// [`IFoo::Greeted`].
    ///
    /// This is the shared storage primitive reused by every version that
    /// supports `greet`; the version-specific behavior (the greeting text, and
    /// whether `greet` exists at all) lives in [`crate::logic`].
    pub fn record_greeting(&mut self, caller: Address, greeting: &str) -> Result<()> {
        // The counter bump and its Greeted event must commit together; guard
        // them with a checkpoint so a failure after the write reverts the
        // advanced counter rather than leaving it advanced without a log.
        let checkpoint = self.storage.checkpoint();

        self.__initialize()?;
        let count = self.greet_count.at(&caller).read()?;
        self.greet_count.at_mut(&caller).write(count.saturating_add(1))?;
        self.emit_event(IFoo::Greeted { caller, greeting: greeting.into() })?;

        checkpoint.commit();
        Ok(())
    }
}
