//! Reference precompile demonstrating hardfork-versioned execution logic.
//!
//! `foo` is a minimal, self-contained example of the execution-consensus
//! versioning design. Business logic is split into immutable, fork-activated
//! versions ([`FooV1`], [`FooV2`]) that are selected by a central version
//! manager ([`FooVersions`]) and routed by an append-only dispatcher.
//!
//! - [`abi`](self) defines the append-only external interface [`IFoo`].
//! - [`storage`](self) holds append-only state ([`FooStorage`]).
//! - [`versions`](self) resolves the version active at a hardfork.
//! - [`logic`](self) contains the frozen per-version implementations.
//! - [`dispatch`](self) decodes calldata and routes to the active version.
//! - [`precompile`](self) is the installable entry point [`Foo`].

mod abi;
pub use abi::IFoo;

mod storage;
pub use storage::FooStorage;

mod versions;
pub use versions::{FooVersion, FooVersions};

mod logic;
pub use logic::{FooLogic, FooV1, FooV2};

mod dispatch;

mod precompile;
pub use precompile::Foo;
