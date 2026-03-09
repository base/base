# `based`

Block building sidecar healthcheck service library.

## Overview

Monitors the health of block builders and proposers by periodically querying their status via
RPC and reporting results as metrics. `BlockProductionHealthChecker` drives the check loop,
`EthClient` handles RPC communication, and `HealthcheckMetrics` exposes Prometheus counters for
operator alerting.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
based = { workspace = true }
```

```rust,ignore
use based::{BlockProductionHealthChecker, HealthcheckConfig};

let config = HealthcheckConfig::from_env()?;
let checker = BlockProductionHealthChecker::new(config);
checker.run().await;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
