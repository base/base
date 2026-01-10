# `base-bundles`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Types for transaction bundles used in Base's flashblocks infrastructure. Provides types for raw bundles, parsed bundles with decoded transactions, accepted bundles with metering data, and bundle cancellation.

## Overview

- **`Bundle`**: Raw bundle type for API requests, mirrors the `eth_sendBundle` format with support for flashblock targeting.
- **`ParsedBundle`**: Decoded bundle with recovered transaction signers, created from raw bundles.
- **`AcceptedBundle`**: Validated and metered bundle ready for inclusion, includes simulation results.
- **`MeterBundleResponse`**: Simulation response containing gas usage, coinbase diff, and per-transaction results.
- **`CancelBundle`**: Request type for cancelling a bundle by its replacement UUID.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-bundles = { git = "https://github.com/base/node-reth" }
```

Parse and validate a bundle:

```rust,ignore
use base_bundles::{Bundle, ParsedBundle, AcceptedBundle, MeterBundleResponse};

// Decode a raw bundle into a parsed bundle with recovered signers
let bundle: Bundle = serde_json::from_str(json)?;
let parsed: ParsedBundle = bundle.try_into()?;

// After metering, create an accepted bundle
let meter_response: MeterBundleResponse = simulate_bundle(&parsed);
let accepted = AcceptedBundle::new(parsed, meter_response);
```

Use bundle extension traits for utility methods:

```rust,ignore
use base_bundles::{ParsedBundle, BundleExtensions};

let parsed: ParsedBundle = bundle.try_into()?;

// Compute bundle hash, get transaction hashes, senders, gas limits, and DA size
let hash = parsed.bundle_hash();
let tx_hashes = parsed.txn_hashes();
let senders = parsed.senders();
let total_gas = parsed.gas_limit();
let da_bytes = parsed.da_size();
```
