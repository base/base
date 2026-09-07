# Rust `uint` crate using const-generics

[![Crates.io Version](https://img.shields.io/crates/v/ruint)](https://crates.io/crates/ruint)
[![docs.rs](https://img.shields.io/docsrs/ruint)](https://docs.rs/ruint)
[![MIT License](https://img.shields.io/github/license/alloy-rs/ruint)](https://github.com/alloy-rs/ruint/blob/main/LICENSE)
[![dependency status](https://deps.rs/repo/github/alloy-rs/ruint/status.svg)](https://deps.rs/repo/github/alloy-rs/ruint)
[![codecov](https://codecov.io/gh/alloy-rs/ruint/branch/main/graph/badge.svg?token=WBPZ9U4TTO)](https://codecov.io/gh/alloy-rs/ruint)
[![CodSpeed Badge](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://codspeed.io/alloy-rs/ruint)
[![CI](https://github.com/alloy-rs/ruint/actions/workflows/ci.yml/badge.svg)](https://github.com/alloy-rs/ruint/actions/workflows/ci.yml)

Implements [`Uint<BITS, LIMBS>`], the ring of numbers modulo $2^{\mathsf{BITS}}$. It requires two
generic arguments: the number of bits and the number of 64-bit 'limbs' required to store those bits.

```rust
# use ruint::Uint;
let answer: Uint<256, 4> = Uint::from(42);
```

You can compute `LIMBS` yourself using $\mathsf{LIMBS} = \left\lceil{\mathsf{BITS} / 64}\right\rceil$,
i.e.`LIMBS` equals `BITS` divided by $64$ rounded up. [`Uint`] will `panic!` if you try to
construct it with incorrect arguments. Ideally this would be a compile time error, but
that is blocked by Rust issue [#60551][r60551].

[r60551]: https://github.com/rust-lang/rust/issues/60551

A more convenient method on stable is to use the [`uint!`] macro, which constructs the right
[`Uint`] for you.

```rust
# use ruint::{Uint, uint};
let answer = uint!(42_U256);
```

You can also use one of the pre-computed type [`aliases`]:

```rust
# use ruint::Uint;
use ruint::aliases::*;

let answer: U256 = Uint::from(42);
```

You can of course also create your own type alias if you need a funny size:

```rust
# use ruint::Uint;
type U1337 = Uint<1337, 21>;

let answer: U1337 = Uint::from(42);
```

## Rust nightly

If you are on nightly, you can use [`Uint<BITS>`][nightly::Uint] which will
compute the number of limbs for you. Unfortunately this can not be made stable
without `generic_const_exprs` support (Rust issue [#76560][r76560]).

[r76560]: https://github.com/rust-lang/rust/issues/76560

```rust
# #[cfg(feature = "generic_const_exprs")] {
use ruint::nightly::Uint;

let answer: Uint<256> = Uint::<256>::from(42);
# }
```

Even on nightly, the ergonomics of Rust are limited. In the example above Rust
requires explicit type annotation for [`Uint::from`], where it did not require
it in the stable version. There are a few more subtle issues that make this
less ideal than it appears. It also looks like it may take some time before
these nightly features are stabilized.

## Examples

```rust
use ruint::Uint;

let a: Uint<256, 4> = Uint::from(0xf00f_u64);
let b: Uint<256, 4> = Uint::from(42_u64);
let c  = a + b;
assert_eq!(c, Uint::from(0xf039_u64));
```

There is a convenient macro [`uint!`] to create constants for you. It allows
for arbitrary length constants using standard Rust integer syntax. The size of
the [`Uint`] or [`Bits`] is specified with a `U` or `B` suffix followed by the
number of bits. The standard Rust syntax of decimal, hexadecimal and even binary and octal is
supported using their prefixes `0x`, `0b` and `0o`. Literals can have
underscores `_` added for readability.

```rust
# use ruint::uint;
let cow = uint!(0xc85ef7d79691fe79573b1a7064c19c1a9819ebdbd1faaab1a8ec92344438aaf4_U256);
```

In fact, this macro recurses down the parse tree, so you can apply it to entire
source files:

```rust
# use ruint::uint;
uint! {

let a = 42_U256;
let b = 0xf00f_1337_c0d3_U256;
let c = a + b;
assert_eq!(c, 263947537596669_U256);

}
```

Note that since `B` is a valid hexadecimal digit there can be ambiguity. To lessen the impact an underscore separator `_B` is required in this case.

## Supported Rust Versions

<!--
When updating this, also update:
- .clippy.toml
- Cargo.toml
- .github/workflows/ci.yml
-->

Uint will keep a rolling MSRV (minimum supported rust version) policy of **at
least** 6 months. When increasing the MSRV, the new Rust version must have been
released at least six months ago. The current MSRV is 1.90.0.

Note that the MSRV is not increased automatically, and only as part of a minor
release.

## Feature flags

There is support for a number of crates. These are enabled by setting the identically
named feature flag.

* [`rand`](https://docs.rs/rand/0.8): Implements sampling from the [`Standard`](https://docs.rs/rand/0.8/rand/distributions/struct.Standard.html) distribution, i.e. [`rng.gen()`](https://docs.rs/rand/0.8/rand/trait.Rng.html#method.gen), alongside some utility `random*` methods on `Uint` itself.
* [`rand-09`](https://docs.rs/rand/0.9): Implements sampling from the [`StandardUniform`](https://docs.rs/rand/0.9/rand/distr/struct.StandardUniform.html) distribution, i.e. [`rng.random()`](https://docs.rs/rand/0.9/rand/trait.Rng.html#method.random), alongside some utility `random*` methods on `Uint` itself.
* [`arbitrary`](https://docs.rs/arbitrary): Implements the [`Arbitrary`](https://docs.rs/arbitrary/latest/arbitrary/trait.Arbitrary.html) trait, allowing [`Uint`]s to be generated for fuzz testing.
* [`quickcheck`](https://docs.rs/quickcheck): Implements the [`Arbitrary`](https://docs.rs/quickcheck/latest/quickcheck/trait.Arbitrary.html) trait, allowing [`Uint`]s to be generated for property based testing.
* [`proptest`](https://docs.rs/proptest): Implements the [`Arbitrary`](https://docs.rs/proptest/latest/proptest/arbitrary/trait.Arbitrary.html) trait, allowing [`Uint`]s to be generated for property based testing. Proptest is used for the `uint`s own test suite.
* [`serde`](https://docs.rs/serde): Implements the [`Serialize`](https://docs.rs/serde/latest/serde/trait.Serialize.html) and [`Deserialize`](https://docs.rs/serde/latest/serde/trait.Deserialize.html) traits for [`Uint`] and [`Bits`].Serialization uses big-endian hex in human readable formats and big-endian byte strings in machine readable formats. [`Uint`] uses ethereum `Quantity` format (0x-prefixed minimal string) when serializing in a human readable format.
* [`rlp`](https://docs.rs/rlp): Implements the [`Encodable`](https://docs.rs/rlp/latest/rlp/trait.Encodable.html) and [`Decodable`](https://docs.rs/rlp/latest/rlp/trait.Decodable.html) traits for [`Uint`] to allow serialization to/from RLP.
* [`fastrlp`](https://docs.rs/fastrlp): Implements the [`Encodable`](https://docs.rs/fastrlp/latest/fastrlp/trait.Encodable.html) and [`Decodable`](https://docs.rs/fastrlp/latest/fastrlp/trait.Decodable.html) traits for [`Uint`] to allow serialization to/from RLP.
* [`primitive-types`](https://docs.rs/primitive-types): Implements the [`From<_>`] conversions between corresponding types.
* [`postgres`](https://docs.rs/postgres): Implements the [`ToSql`](https://docs.rs/postgres/latest/postgres/types/trait.ToSql.html) trait supporting many column types.
* [`num-bigint`](https://docs.rs/num-bigint): Implements conversion to/from [`BigUint`](https://docs.rs/num-bigint/latest/num_bigint/struct.BigUint.html) and [`BigInt`](https://docs.rs/num-bigint/latest/num_bigint/struct.BigInt.html).
* [`bigdecimal`](https://docs.rs/bigdecimal) Implements conversion to/from [`BigDecimal`](https://docs.rs/bigdecimal/latest/bigdecimal/struct.BigDecimal.html).
* [`ark-ff`](https://docs.rs/ark-ff): Implements conversion to/from the [`BigInteger*`](https://docs.rs/ark-ff/0.3.0/ark_ff/biginteger/index.html) types and the [`Fp*`](https://docs.rs/ark-ff/0.3.0/ark_ff/fields/models/index.html) types from `ark-ff@0.3`.
* [`ark-ff-04`](https://docs.rs/ark-ff): Implements conversion to/from [`BigInt`](https://docs.rs/ark-ff/0.4.2/ark_ff/biginteger/struct.BigInt.html) and [`Fp`](https://docs.rs/ark-ff/0.4.2/ark_ff/fields/models/fp/struct.Fp.html) types from `ark-ff@0.4`.
* [`ark-ff-05`](https://docs.rs/ark-ff): Implements conversion to/from [`BigInt`](https://docs.rs/ark-ff/0.5.0/ark_ff/biginteger/struct.BigInt.html) and [`Fp`](https://docs.rs/ark-ff/0.5.0/ark_ff/fields/models/fp/struct.Fp.html) types from `ark-ff@0.5`.
* [`ark-ff-06`](https://docs.rs/ark-ff): Implements conversion to/from [`BigInt`](https://docs.rs/ark-ff/0.6.0/ark_ff/biginteger/struct.BigInt.html) and [`Fp`](https://docs.rs/ark-ff/0.6.0/ark_ff/fields/models/fp/struct.Fp.html) types from `ark-ff@0.5`.
* [`sqlx`](https://docs.rs/sqlx): Implements database agnostic storage as byte array. Requires
  `sqlx` to be used with the `tokio-native-tls` runtime, due to issue [sqlx#1627](https://github.com/launchbadge/sqlx/issues/1627).
* [`zeroize`](https://docs.rs/zeroize): Implements the [`Zeroize`](https://docs.rs/zeroize/latest/zeroize/trait.Zeroize.html) trait. This makes [`Uint`] and [`Bits`] compatible with the [`secrecy`](https://crates.io/crates/secrecy) crate.
* [`valuable`](https://docs.rs/valuable): Implements the [`Valuable`](https://docs.rs/valuable/0.1.0/valuable/trait.Valuable.html) trait.
* [`pyo3`](https://docs.rs/pyo3): Implements the [`IntoPyObject`](https://docs.rs/pyo3/latest/pyo3/conversion/trait.IntoPyObject.html) and [`FromPyObject`](https://docs.rs/pyo3/latest/pyo3/conversion/trait.FromPyObject.html) traits.
* [`parity-scale-codec`](https://docs.rs/parity-scale-codec): Implements the [`Encode`](https://docs.rs/parity-scale-codec/latest/parity_scale_codec/trait.Encode.html), [`Decode`](https://docs.rs/parity-scale-codec/latest/parity_scale_codec/trait.Decode.html), [`MaxEncodedLen`](https://github.com/paritytech/parity-scale-codec/blob/47d98a1c23dabc890fdb548d115a18070082c66e/src/max_encoded_len.rs) and [`HasCompact`](https://docs.rs/parity-scale-codec/latest/parity_scale_codec/trait.HasCompact.html) traits.
* [`bn-rs`](https://docs.rs/bn-rs/latest/bn_rs/): Implements conversion to/from the [`BN`](https://docs.rs/bn-rs/latest/bn_rs/struct.BN.html) and [`BigNumber`](https://docs.rs/bn-rs/latest/bn_rs/struct.BigNumber.html).
* [`bytemuck`](https://docs.rs/bytemuck): Implements the [`Pod`](https://docs.rs/bytemuck/latest/bytemuck/trait.Pod.html) and [`Zeroable`](https://docs.rs/bytemuck/latest/bytemuck/trait.Zeroable.html) traits for [`Uint`] where the size is a multiple of 64, up to 1024. This allows `Uint` to be used where a `Pod` trait bound exists.
* [`num-traits`](https://docs.rs/num-traits): Implements about forty applicable traits.
* [`subtle`](https://docs.rs/subtle): Implements [`Uint::bit_ct`], [`ConditionallySelectable`](https://docs.rs/subtle/latest/subtle/trait.ConditionallySelectable.html),[`ConditionallyNegatable`](https://docs.rs/subtle/latest/subtle/trait.ConditionallyNegatable.html), [`ConstantTimeEq`](https://docs.rs/subtle/latest/subtle/trait.ConstantTimeEq.html)/[`ConstantTimeGreater`](https://docs.rs/subtle/latest/subtle/trait.ConstantTimeGreater.html)/[`ConstantTimeLess`](https://docs.rs/subtle/latest/subtle/trait.ConstantTimeLess.html).
* [`der`](https://docs.rs/der): Implements [`Encode`](https://docs.rs/der/latest/der/trait.Encode.html)/[`Decode`](https://docs.rs/der/latest/der/trait.Decode.html) and [`TryFrom`]/[`From`] casting for [`Any`](https://docs.rs/der/latest/der/asn1/struct.Any.html), [`AnyRef`](https://docs.rs/der/latest/der/asn1/struct.AnyRef.html), [`Int`](https://docs.rs/der/latest/der/asn1/struct.Int.html), [`IntRef`](https://docs.rs/der/latest/der/asn1/struct.IntRef.html), [`Uint`](https://docs.rs/der/latest/der/asn1/struct.Uint.html), [`UintRef`](https://docs.rs/der/latest/der/asn1/struct.UintRef.html).
* [`diesel`](https://docs.rs/diesel): Implements the [`ToSql`](https://docs.rs/diesel/latest/diesel/serialize/trait.ToSql.html) and [`FromSql`](https://docs.rs/diesel/latest/diesel/deserialize/trait.FromSql.html) traits for storing `Uint` values as byte arrays in databases supported by Diesel.
* [`rkyv`](https://docs.rs/rkyv/): Implements the [`Archive`](https://docs.rs/rkyv/latest/rkyv/trait.Archive.html), [`Serialize`](https://docs.rs/rkyv/latest/rkyv/trait.Serialize.html), [`Deserialize`](https://docs.rs/rkyv/latest/rkyv/trait.Deserialize.html) and [`Portable`](https://docs.rs/rkyv/latest/rkyv/trait.Portable.html) traits for `Uint` and `Bits`.
  Implements [`ArchivedUint`](https://docs.rs/ruint/latest/ruint/support/rkyv/struct.ArchivedUint.html) and [`ArchivedBits`](https://docs.rs/ruint/latest/ruint/support/rkyv/struct.ArchivedBits.html) types that can be used to access `Uint` and `Bits` values from an archive without needing to allocate new memory.
  This allows for zero-copy deserialization of `Uint` and `Bits` values.

## Building and testing

Format, lint, build and test everything (I recommend creating a shell alias for this):

```sh
cargo fmt &&\
cargo clippy --all-features --all-targets &&\
cargo test --workspace --all-features --doc -- --nocapture &&\
cargo test --workspace --all-features --all-targets -- --nocapture &&\
cargo doc --workspace --all-features --no-deps
```

Run benchmarks with the provided `.cargo/config.toml` alias

```sh
cargo criterion
```

Check documentation coverage

```sh
RUSTDOCFLAGS="-Z unstable-options --show-coverage"  cargo doc --workspace --all-features --no-deps
```

## Features

* All the quality of life features one could want.
* Compatible with std `u64`, etc types. See Rust's [integer methods](https://doc.rust-lang.org/stable/std/primitive.u64.html).
* Adhere to [Rust API Guidelines](https://rust-lang.github.io/api-guidelines)
* Montgomery REDC and other algo's for implementing prime fields.
* **Community maintained**. Support for use in foreign bindings for Swift and Kotlin though [ruint-uniffi](https://github.com/paolodamico/ruint-uniffi) crate.

## To do

* Builds `no-std` and `wasm`.
* Fast platform agnostic generic algorithms.
* Target specific assembly optimizations (where available).
* Optional num-traits, etc, support.
* Run-time sized type with compatible interface.

---

[![lines of code](https://img.shields.io/tokei/lines/github/alloy-rs/ruint)](https://github.com/alloy-rs/ruint)
[![GitHub contributors](https://img.shields.io/github/contributors/alloy-rs/ruint)](https://github.com/alloy-rs/ruint/graphs/contributors)
[![GitHub issues](https://img.shields.io/github/issues/alloy-rs/ruint)](https://github.com/alloy-rs/ruint/issues)
[![GitHub pull requests](https://img.shields.io/github/issues-pr/alloy-rs/ruint?label=PRs)](https://github.com/alloy-rs/ruint/pulls)
[![GitHub Repo stars](https://img.shields.io/github/stars/alloy-rs/ruint)](https://star-history.com/#alloy-rs/ruint&Date)
[![crates.io](https://img.shields.io/crates/d/ruint)](https://crates.io/crates/ruint)
