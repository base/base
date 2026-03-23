# Load Tests

Load testing and benchmarking framework for Base infrastructure.

## Workspace Structure

```
load-tests/
├── base-load-tests-core/   # Core library - all business logic
```

## Crates

| Crate | Description | Status |
|-------|-------------|--------|
| `base-load-tests-core` | Core library with workload generation, network management, metrics | MVP Complete |

## Goals

- Provide standardized transaction spamming for network load testing
- Centralize workload generation, network orchestration, and metrics collection
- Enable reproducible test scenarios with deterministic configurations

## Quick Start

```bash
# Build the workspace
cargo build -p base-load-tests-core

# Run tests
cargo test -p base-load-tests-core

# Run the spam example with a YAML config file
cargo run -p base-load-tests-core --example spam -- path/to/config.yaml

# Or use the default config (examples/config.yaml)
cargo run -p base-load-tests-core --example spam
```

## Configuration

All configuration is done via YAML files. See `base-load-tests-core/examples/config.yaml` for a fully documented example.

Example minimal config (`devnet.yaml`):

```yaml
rpc: http://localhost:8545
sender_count: 10
target_gps: 2100000
duration: "30s"
```

Note: Set `FUNDER_KEY` environment variable with a funded private key (0x-prefixed hex).
