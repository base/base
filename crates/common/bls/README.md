# `base-common-bls`

BLS12-381 signature verification and aggregation for native BLS transactions.

## Overview

`BlsCrypto` implements the minimal-signature-size proof-of-possession ciphersuites used by native
BLS transactions: public keys are compressed G2 points (96 bytes) and signatures are compressed G1
points (48 bytes). It decodes points with full canonical, curve and prime-order subgroup
validation, hashes messages to G1 (RFC 9380), verifies proofs of possession and aggregate
signatures with a single multi-pairing, and aggregates already-aggregated signatures.

The crate is `no_std` and backed by arkworks, so payload building, block execution and fault or
validity proofs share one deterministic implementation.

Aggregate verification over repeated or attacker-chosen messages is only sound for keys whose
proof of possession was verified; callers must only verify against registered keys.

## Usage

```toml
[dependencies]
base-common-bls = { workspace = true }
```

```rust,ignore
use base_common_bls::BlsCrypto;

let aggregate = BlsCrypto::aggregate_signatures(&[first, second])?;
```
