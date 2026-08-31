# `base-bundles`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Types for transaction batches used by Base resource metering. Provides types for raw transaction lists, parsed batches with decoded transactions, accepted batches with metering data, and simulation results.

## Overview

- **`Bundle`**: Raw transaction-batch container used as input to `base_meterBundle`.
- **`ParsedBundle`**: Decoded batch with recovered transaction signers, created from raw batches.
- **`AcceptedBundle`**: Validated and metered batch, includes simulation results.
- **`MeterBundleResponse`**: Simulation response containing gas usage, coinbase diff, and per-transaction results.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-bundles = { git = "https://github.com/base/base" }
```

Parse and meter a batch:

```rust,ignore
use base_bundles::{Bundle, ParsedBundle, AcceptedBundle, MeterBundleResponse};

// Decode a raw batch into recovered signers
let bundle: Bundle = serde_json::from_str(json)?;
let parsed: ParsedBundle = bundle.try_into()?;

// After metering, create an accepted batch
let meter_response: MeterBundleResponse = simulate_bundle(&parsed);
let accepted = AcceptedBundle::new(parsed, meter_response);
```

Use extension traits for utility methods:

```rust,ignore
use base_bundles::{ParsedBundle, BundleExtensions};

let parsed: ParsedBundle = bundle.try_into()?;

// Compute batch hash, get transaction hashes, senders, gas limits, and DA size
let hash = parsed.bundle_hash();
let tx_hashes = parsed.txn_hashes();
let senders = parsed.senders();
let total_gas = parsed.gas_limit();
let da_bytes = parsed.da_size();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
