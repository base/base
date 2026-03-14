# base-load-tests-core

Core library for Base network load testing and benchmarking.

## Overview

`base-load-tests-core` provides foundational components for transaction spamming and
load testing across Base infrastructure. It consolidates workload generation,
transaction submission, and metrics collection into a single reusable library.

## Architecture

This crate follows a library-first design where all business logic resides here. An
example spammer is provided in `examples/spam.rs`.

### Modules

- `config/` - Configuration types for networks and workloads
- `network/` - Network lifecycle management (devnet, remote connections)
- `workload/` - Transaction payload generation (transfers, ERC20, Uniswap, storage ops)
- `runner/` - Load test execution and rate limiting
- `metrics/` - Latency, throughput, and gas usage collection
- `rpc/` - RPC client abstractions and transaction submission

## Usage

```rust,ignore
use base_load_tests_core::{LoadConfig, LoadRunner};

let config = LoadConfig::new()
    .rpc_url("http://localhost:8545".parse().unwrap())
    .chain_id(1337)
    .tps(10)
    .duration(std::time::Duration::from_secs(30));

let mut runner = LoadRunner::new(config).unwrap();
let summary = runner.run().await.unwrap();
println!("Submitted: {}, Confirmed: {}", summary.throughput.submitted, summary.throughput.confirmed);
```

## License

MIT
