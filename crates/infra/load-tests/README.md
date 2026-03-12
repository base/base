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

# Run the spam example against devnet
cargo run -p base-load-tests-core --example spam -- \
  http://localhost:8545 \
  0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d \
  100 30 10
```
