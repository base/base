# `base-proof-host`

Generic host-side infrastructure for the Base fault proof host.

## Overview

Manages the host side of the preimage oracle protocol for fault proof generation. The `Host`
orchestrator runs the preimage oracle server, serving requests from FPVM client programs via
`PreimageOracleServer`. Key-value stores (`MemoryKeyValueStore`, `DiskKeyValueStore`,
`BootKeyValueStore`) cache preimages fetched from L1/L2 RPCs. Supports online mode (fetching
data from live nodes) and offline mode (reading from a pre-populated store), and exposes
`RecordingOracle` for capturing preimage access traces.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-host = { workspace = true }
```

```rust,ignore
use base_proof_host::{Host, HostConfig, OnlineHostBackend};

let config = HostConfig::from_cli(args)?;
let host = Host::new(config, OnlineHostBackend::new(providers));
host.run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
