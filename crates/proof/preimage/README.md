# `base-proof-preimage`

High-level `no_std`-compatible API over the [Preimage Oracle][preimage-abi-spec].

## Overview

Provides client and host abstractions for the fault proof preimage oracle protocol. Client-side
traits (`PreimageOracleClient`, `HintWriterClient`) allow `no_std` FPVM programs to fetch
preimages and write hints. Host-side traits (`PreimageOracleServer`, `HintReaderServer`,
`HintRouter`, `PreimageFetcher`) are `async`-colored to allow host programs to fetch data from
external sources and serve it to the client over pipes or shared memory.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-preimage = { workspace = true }
```

```rust,ignore
use base_proof_preimage::{PreimageOracleClient, HintWriterClient};

// In a no_std FPVM client:
let data = oracle.get(preimage_key)?;
hint_writer.write(&hint)?;
```

[preimage-abi-spec]: https://specs.base.org/protocol/fault-proof#pre-image-oracle

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
