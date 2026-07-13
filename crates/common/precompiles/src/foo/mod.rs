//! Reference precompile demonstrating hardfork-versioned execution logic.
//!
//! Pragmatic-split variant of the versioning design: the **ABI and storage are
//! shared** because they are cross-version invariants (a selector must always
//! decode the same way; every version reads/writes the same state slots), while
//! each **version is self-contained** and owns its own selector routing and
//! business logic behind the single [`FooVersion`] seam.
//!
//! - [`abi`](self) — shared, append-only external interface [`IFoo`].
//! - [`storage`](self) — shared, append-only state [`FooStorage`].
//! - [`versions`](self) — the [`FooVersion`] seam and the [`FooVersions`] resolver.
//! - [`logic`](self) — self-contained per-version implementations.
//! - [`dispatch`](self) — shared entry: charges calldata gas and hands off.
//! - [`precompile`](self) — the installable entry point [`Foo`].

mod abi;
pub use abi::IFoo;

mod storage;
pub use storage::FooStorage;

mod versions;
pub use versions::{FooVersion, FooVersions};

mod logic;
pub use logic::{FooV1, FooV2};

mod dispatch;

mod precompile;
pub use precompile::Foo;
