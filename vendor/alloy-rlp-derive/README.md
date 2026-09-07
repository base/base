# alloy-rlp-derive

This crate provides derive macros for traits defined in
[`alloy-rlp`](https://docs.rs/alloy-rlp). See that crate's documentation for
more information.

This library also supports up to 1 `#[rlp(default)]` in a struct, which is
similar to [`#[serde(default)]`](https://serde.rs/field-attrs.html#default)
with the caveat that we use the `Default` value if the field deserialization
fails, as we don't serialize field names and there is no way to tell if it is
present or not.

Optional trailing fields can be enabled with `#[rlp(trailing)]`.

- `#[rlp(trailing)]` allows trailing `Option<_>` fields and uses `0x80` as the
  placeholder for interior `None` values when a later optional field is present.
- `#[rlp(trailing(no_gaps))]` omits every `None` optional field. Consumers must
  ensure there are no gaps, meaning an optional field cannot be `None` if any
  later optional field is `Some`.
- `#[rlp(trailing(canonical))]` keeps the usual interior `0x80` placeholder but
  requires trailing `None` values to be omitted rather than encoded as `0x80`.

These options are contract-style APIs: the derive macros do not enforce the
`no_gaps` or `canonical` invariants at runtime in release builds, so callers are
responsible for constructing values that satisfy them.
