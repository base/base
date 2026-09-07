enr
============

[![Build Status]][Build Link] [![Doc Status]][Doc Link] [![Crates
Status]][Crates Link]

[Build Status]: https://github.com/AgeManning/enr/workflows/build/badge.svg?branch=master
[Build Link]: https://github.com/AgeManning/enr/actions
[Doc Status]: https://docs.rs/enr/badge.svg
[Doc Link]: https://docs.rs/enr
[Crates Status]: https://img.shields.io/crates/v/enr.svg
[Crates Link]: https://crates.io/crates/enr

[Documentation at docs.rs](https://docs.rs/enr)

This crate contains an implementation of an Ethereum Node Record (ENR) as specified by
[EIP-778](https://eips.ethereum.org/EIPS/eip-778) extended to allow for the use of ed25519 keys.

An ENR is a signed, key-value record which has an associated `NodeId` (a 32-byte identifier).
Updating/modifying an ENR requires an `EnrKey` in order to re-sign the record with the
associated key-pair.

ENR's are identified by their sequence number. When updating an ENR, the sequence number is
increased.

Different identity schemes can be used to define the node id and signatures. Currently only the
"v4" identity is supported and is set by default.

## Signing Algorithms

User's wishing to implement their own signing algorithms simply need to
implement the `EnrKey` trait and apply it to an `Enr`.

By default, `k256::SigningKey` implement `EnrKey` and can be used to sign and
verify ENR records. This library also implements `EnrKey` for `ed25519_dalek::Keypair` via the `ed25519`
feature flag.

Furthermore, a `CombinedKey` is provided if the `ed25519` feature flag is set, which provides an
ENR type that can support both `secp256k1` and `ed25519` signed ENR records. Examples of the
use of each of these key types is given below.

## Features

This crate supports a number of features.

- `serde`: Allows for serde serialization and deserialization for ENRs.
- `ed25519`: Provides support for `ed25519_dalek` keypair types.
- `rust-secp256k1`: Uses `c-secp256k1` for secp256k1 keys.

These can be enabled via adding the feature flag in your `Cargo.toml`

```toml
enr = { version = "*", features = ["serde", "ed25519", "rust-secp256k1"] }
```

## Examples

#### Building an ENR with the default `k256` key type

```rust
use enr::{Enr, k256};
use std::net::Ipv4Addr;
use rand::thread_rng;

// generate a random secp256k1 key
let mut rng = thread_rng();
let key = k256::ecdsa::SigningKey::random(&mut rng);

let ip = Ipv4Addr::new(192,168,0,1);
let enr = Enr::builder().ip4(ip).tcp4(8000).build(&key).unwrap();

assert_eq!(enr.ip4(), Some("192.168.0.1".parse().unwrap()));
assert_eq!(enr.id(), Some("v4".into()));
```

#### Building an ENR with the `CombinedKey` type (support for multiple signing algorithms).

Note the `ed25519` feature flag must be set. This makes use of the
`builder::Builder` struct.

```rust
use enr::{Enr, CombinedKey};
use std::net::Ipv4Addr;

// create a new secp256k1 key
let key = CombinedKey::generate_secp256k1();

// or create a new ed25519 key
let key = CombinedKey::generate_ed25519();

let ip = Ipv4Addr::new(192,168,0,1);
let enr = Enr::builder().ip4(ip).tcp4(8000).build(&key).unwrap();

assert_eq!(enr.ip4(), Some("192.168.0.1".parse().unwrap()));
assert_eq!(enr.id(), Some("v4".into()));
```

#### Modifying an ENR

ENR fields can be added and modified using the getters/setters on `Enr`. A custom field
can be added using `insert` and retrieved with `get`.

```rust
use enr::{k256::ecdsa::SigningKey, Enr};
use std::net::Ipv4Addr;
use rand::thread_rng;

// specify the type of ENR
type DefaultEnr = Enr<SigningKey>;

// generate a random secp256k1 key
let mut rng = thread_rng();
let key = SigningKey::random(&mut rng);

let ip = Ipv4Addr::new(192,168,0,1);
let mut enr = Enr::builder().ip4(ip).tcp4(8000).build(&key).unwrap();

enr.set_tcp4(8001, &key);
// set a custom key
enr.insert("custom_key", &[0,0,1], &key);

// encode to base64
let base_64_string = enr.to_base64();

// decode from base64
let decoded_enr: DefaultEnr = base_64_string.parse().unwrap();

assert_eq!(decoded_enr.ip4(), Some("192.168.0.1".parse().unwrap()));
assert_eq!(decoded_enr.id(), Some("v4".into()));
assert_eq!(decoded_enr.tcp4(), Some(8001));
assert_eq!(decoded_enr.get("custom_key").as_ref().map(AsRef::as_ref), Some(vec![0,0,1]).as_deref());
```

#### Encoding/Decoding ENR's of various key types

```rust
use enr::{ed25519_dalek as ed25519, k256::ecdsa, CombinedKey, Enr};
use std::net::Ipv4Addr;
use rand::thread_rng;

// generate a random secp256k1 key
let mut rng = thread_rng();
let key = ecdsa::SigningKey::random(&mut rng);
let ip = Ipv4Addr::new(192, 168, 0, 1);
let enr_secp256k1 = Enr::builder().ip4(ip).tcp4(8000).build(&key).unwrap();

// encode to base64
let base64_string_secp256k1 = enr_secp256k1.to_base64();

// generate a random ed25519 key
let key = ed25519::SigningKey::generate(&mut rng);
let enr_ed25519 = Enr::builder().ip4(ip).tcp4(8000).build(&key).unwrap();

// encode to base64
let base64_string_ed25519 = enr_ed25519.to_base64();

// decode base64 strings of varying key types
// decode the secp256k1 with default Enr
let decoded_enr_secp256k1: Enr<ecdsa::SigningKey> = base64_string_secp256k1.parse().unwrap();
// decode ed25519 ENRs
let decoded_enr_ed25519: Enr<ed25519::SigningKey> = base64_string_ed25519.parse().unwrap();

// use the combined key to be able to decode either
let decoded_enr: Enr<CombinedKey> = base64_string_secp256k1.parse().unwrap();
let decoded_enr: Enr<CombinedKey> = base64_string_ed25519.parse().unwrap();
```
