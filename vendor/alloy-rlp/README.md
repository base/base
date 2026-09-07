# alloy-rlp

This crate provides Ethereum RLP (de)serialization functionality. RLP is
commonly used for Ethereum EL datastructures, and its documentation can be
found [at ethereum.org][ref].

[ref]: https://ethereum.org/en/developers/docs/data-structures-and-encoding/rlp/

## Usage

We strongly recommend deriving RLP traits via the `RlpEncodable` and
`RlpDecodable` derive macros.

Trait methods can then be accessed via the `Encodable` and `Decodable` traits.

## Trailing Optional Fields

The derive macros support trailing `Option<_>` fields via `#[rlp(trailing)]`.
This enables the common RLP pattern where optional fields may be omitted from the
end of a list.

Two stricter variants are also available:

- `#[rlp(trailing(no_gaps))]` omits every `None` optional field, including
  interior ones. This only works if consumers construct values without gaps,
  meaning no later optional field may be `Some` after an earlier one is `None`.
- `#[rlp(trailing(canonical))]` preserves interior `None` placeholders when a
  later optional field is present, but trailing `None` values must still be
  omitted to remain canonical.

These modes do not perform runtime validation in release builds. They describe
encoding invariants that callers must uphold when constructing values.

## Example

```rust
# #[cfg(feature = "derive")] {
use alloy_rlp::{RlpEncodable, RlpDecodable, Decodable, Encodable};

#[derive(Debug, RlpEncodable, RlpDecodable, PartialEq)]
pub struct MyStruct {
    pub a: u64,
    pub b: Vec<u8>,
}

let my_struct = MyStruct {
    a: 42,
    b: vec![1, 2, 3],
};

let mut buffer = Vec::<u8>::new();
let encoded = my_struct.encode(&mut buffer);
let decoded = MyStruct::decode(&mut buffer.as_slice()).unwrap();
assert_eq!(my_struct, decoded);
# }
```
