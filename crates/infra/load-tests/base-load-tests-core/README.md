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

Load configuration from a YAML file:

```rust,ignore
use base_load_tests_core::{LoadRunner, RpcClient, TestConfig};

// Load config from YAML
let test_config = TestConfig::load("config.yaml")?;

// Fetch chain ID from RPC if not specified in config
let client = RpcClient::new(test_config.rpc.parse()?);
let chain_id = test_config.chain_id.or(Some(client.chain_id().await?));

// Convert to runtime config
let load_config = test_config.to_load_config(chain_id)?;

// Run the load test
let mut runner = LoadRunner::new(load_config)?;
let summary = runner.run().await?;
println!("Submitted: {}, Confirmed: {}", summary.throughput.total_submitted, summary.throughput.total_confirmed);
```

See `examples/config.yaml` for all available configuration options.

## License

MIT
