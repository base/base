# Development Guide

This guide provides detailed information for developers who want to contribute to or build upon Base Reth Node.

## Table of Contents

- [Getting Started](#getting-started)
- [Project Structure](#project-structure)
- [Development Environment](#development-environment)
- [Building and Testing](#building-and-testing)
- [Code Style and Standards](#code-style-and-standards)
- [Debugging](#debugging)
- [Common Development Tasks](#common-development-tasks)
- [Performance Profiling](#performance-profiling)
- [Troubleshooting](#troubleshooting)

## Getting Started

### Prerequisites

Ensure you have the following installed:

```bash
# Rust (version 1.88 or later)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update

# Just (command runner)
cargo install just

# Docker (optional, for containerized development)
# Follow instructions at https://docs.docker.com/get-docker/

# Build essentials (Linux)
sudo apt-get update
sudo apt-get install -y git libclang-dev pkg-config curl build-essential
```

### Initial Setup

```bash
# Clone the repository
git clone https://github.com/base/base.git
cd base

# Build the project
just build

# Run tests to verify setup
just test
```

## Project Structure

```
base/
├── bin/                    # Binary crates (executables)
│   ├── builder/           # Block builder binary
│   ├── consensus/         # Consensus binary
│   └── node/              # Main node binary
├── crates/                # Library crates
│   ├── builder/           # Builder implementation
│   ├── client/            # Client components
│   ├── consensus/         # Consensus components
│   └── shared/            # Shared utilities
├── devnet/                # Development network setup
├── docker/                # Docker configurations
├── docs/                  # Documentation
├── scripts/               # Build and deployment scripts
├── Cargo.toml             # Workspace configuration
├── Justfile               # Task runner commands
└── README.md              # Project overview
```

### Key Directories

#### `bin/`
Contains executable binaries:
- **node**: Main Base Reth Node executable
- **builder**: Standalone block builder
- **consensus**: Consensus client

#### `crates/client/`
Client-side components:
- **engine**: Execution engine integration
- **flashblocks**: Flashblock client implementation
- **flashblocks-node**: Node with flashblock support
- **metering**: Resource metering
- **proofs**: Proof generation
- **txpool**: Transaction pool

#### `crates/shared/`
Shared libraries:
- **access-lists**: Flashblock-level access lists
- **bundles**: Transaction bundles
- **flashtypes**: Flashblock type definitions
- **primitives**: Core primitives
- **node**: Node utilities

## Development Environment

### Recommended IDE Setup

#### Visual Studio Code

Install the following extensions:
- `rust-analyzer`: Rust language support
- `CodeLLDB`: Debugging support
- `Even Better TOML`: TOML file support
- `Error Lens`: Inline error display

#### Rust Analyzer Configuration

Add to `.vscode/settings.json`:

```json
{
  "rust-analyzer.cargo.features": "all",
  "rust-analyzer.checkOnSave.command": "clippy",
  "rust-analyzer.checkOnSave.extraArgs": ["--all-targets"],
  "editor.formatOnSave": true,
  "[rust]": {
    "editor.defaultFormatter": "rust-lang.rust-analyzer"
  }
}
```

### Environment Variables

For development, you can use the provided `.env.devnet` file:

```bash
# Copy and modify as needed
cp .env.devnet .env.local

# Source the environment
source .env.local
```

## Building and Testing

### Build Commands

```bash
# Standard release build
just build

# Debug build (faster compilation, slower runtime)
cargo build

# Maximum performance build (slower compilation, fastest runtime)
just build-maxperf

# Build specific binary
cargo build --release --bin base-reth-node
cargo build --release --bin base-builder
cargo build --release --bin base-consensus

# Build with all features
cargo build --all-features

# Build documentation
cargo doc --no-deps --open
```

### Testing

```bash
# Run all tests
just test

# Run tests for a specific crate
cargo test -p base-flashblocks

# Run tests with output
cargo test -- --nocapture

# Run specific test
cargo test test_name

# Run tests with logging
RUST_LOG=debug cargo test

# Run integration tests
cargo test --test '*'

# Run doc tests
cargo test --doc
```

### Code Quality Checks

```bash
# Run all checks (formatting + clippy)
just check

# Format code
cargo fmt

# Check formatting without modifying
cargo fmt --check

# Run clippy
cargo clippy --all-targets --all-features

# Run clippy with auto-fix
cargo clippy --fix --all-targets --all-features

# Auto-fix formatting and clippy issues
just fix

# Full CI suite (what runs in CI)
just ci
```

### Benchmarking

```bash
# Run benchmarks
cargo bench

# Run specific benchmark
cargo bench --bench benchmark_name

# Generate flamegraph
cargo flamegraph --bench benchmark_name
```

## Code Style and Standards

### Rust Style Guidelines

This project follows the official Rust style guidelines with some additions:

1. **Formatting**: Use `rustfmt` with the project's `rustfmt.toml`
2. **Linting**: All clippy warnings must be addressed
3. **Documentation**: All public APIs must have documentation
4. **Error Handling**: Use `Result` and proper error types, avoid `unwrap()` in production code

### Clippy Configuration

The project uses strict clippy lints defined in `clippy.toml` and `Cargo.toml`:

```toml
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
missing-const-for-fn = "warn"
redundant-clone = "warn"
# ... see clippy.toml for full list
```

### Documentation Standards

```rust
/// Brief one-line description.
///
/// More detailed explanation of what this function does,
/// including any important behavior or edge cases.
///
/// # Arguments
///
/// * `param1` - Description of param1
/// * `param2` - Description of param2
///
/// # Returns
///
/// Description of return value
///
/// # Errors
///
/// Description of when/why this function returns an error
///
/// # Examples
///
/// ```
/// use base_example::function;
///
/// let result = function(arg1, arg2);
/// assert_eq!(result, expected);
/// ```
pub fn function(param1: Type1, param2: Type2) -> Result<ReturnType, ErrorType> {
    // Implementation
}
```

### Commit Message Convention

Follow the Conventional Commits specification:

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `chore`: Maintenance tasks
- `refactor`: Code refactoring
- `test`: Test additions or changes
- `perf`: Performance improvements

**Examples:**
```
feat(flashblocks): add websocket publisher for real-time updates

fix(txpool): resolve race condition in transaction ordering

docs(specs): clarify access list generation rules

chore(deps): update reth to v1.2.3
```

## Debugging

### Logging

The project uses `tracing` for structured logging:

```rust
use tracing::{debug, info, warn, error, trace};

// In your code
debug!("Processing transaction: {:?}", tx);
info!("Flashblock generated: index={}", index);
warn!("High memory usage: {} MB", memory_mb);
error!("Failed to execute transaction: {}", err);
```

**Setting log levels:**

```bash
# Set global log level
RUST_LOG=debug cargo run

# Set per-module log level
RUST_LOG=base_flashblocks=debug,base_builder=info cargo run

# Log to file
RUST_LOG=debug cargo run 2>&1 | tee output.log
```

### Using a Debugger

#### LLDB (recommended for macOS/Linux)

```bash
# Build with debug symbols
cargo build

# Run with lldb
rust-lldb target/debug/base-reth-node

# Set breakpoint
(lldb) breakpoint set --name function_name
(lldb) breakpoint set --file file.rs --line 42

# Run
(lldb) run --arg1 value1

# Common commands
(lldb) continue    # Continue execution
(lldb) step        # Step into
(lldb) next        # Step over
(lldb) print var   # Print variable
(lldb) bt          # Backtrace
```

#### GDB (Linux)

```bash
# Build with debug symbols
cargo build

# Run with gdb
rust-gdb target/debug/base-reth-node

# Similar commands to lldb
(gdb) break function_name
(gdb) run
(gdb) continue
(gdb) step
(gdb) next
(gdb) print var
(gdb) backtrace
```

### Memory Profiling

```bash
# Install valgrind (Linux)
sudo apt-get install valgrind

# Run with valgrind
valgrind --leak-check=full --show-leak-kinds=all \
  target/release/base-reth-node [args]

# Use heaptrack for heap profiling
heaptrack target/release/base-reth-node [args]
heaptrack_gui heaptrack.base-reth-node.*.gz
```

## Common Development Tasks

### Adding a New RPC Method

1. Define the method in the appropriate RPC trait:

```rust
// In crates/client/flashblocks/src/rpc.rs
#[async_trait]
pub trait FlashblocksRpcApi {
    /// Get flashblock by index
    async fn get_flashblock_by_index(&self, index: u64) -> Result<Option<Flashblock>>;
}
```

2. Implement the method:

```rust
#[async_trait]
impl FlashblocksRpcApi for FlashblocksRpc {
    async fn get_flashblock_by_index(&self, index: u64) -> Result<Option<Flashblock>> {
        // Implementation
    }
}
```

3. Register the RPC method in the server setup
4. Add tests
5. Update documentation

### Adding a New Crate

1. Create the crate directory:

```bash
mkdir -p crates/category/new-crate
cd crates/category/new-crate
cargo init --lib
```

2. Update `Cargo.toml`:

```toml
[package]
name = "base-new-crate"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
# Add dependencies
```

3. Add to workspace in root `Cargo.toml`:

```toml
[workspace]
members = [
    # ...
    "crates/category/new-crate",
]
```

4. Add README.md and documentation

### Modifying Flashblock Generation

Flashblock generation logic is in `crates/builder/core/src/flashblocks/generator.rs`:

```rust
// Key function to modify
pub async fn generate_flashblock(&mut self) -> Result<Flashblock> {
    // Your modifications here
}
```

### Working with Access Lists

Access list generation is in `crates/shared/access-lists/`:

```rust
use base_fbal::{FBALBuilderDb, FlashblockAccessListBuilder};

// Wrap your database
let mut fbal_db = FBALBuilderDb::new(db);

// Set transaction index before execution
fbal_db.set_index(tx_index);

// Execute transaction
// ...

// Commit changes
fbal_db.commit(state_changes);

// Build access list
let builder = fbal_db.finish()?;
let access_list = builder.build(min_tx_index, max_tx_index);
```

## Performance Profiling

### CPU Profiling with perf (Linux)

```bash
# Record performance data
perf record -g target/release/base-reth-node [args]

# View report
perf report

# Generate flamegraph
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg
```

### Using cargo-flamegraph

```bash
# Install
cargo install flamegraph

# Generate flamegraph
cargo flamegraph --bin base-reth-node -- [args]

# Open flamegraph.svg in browser
```

### Benchmarking

```bash
# Run criterion benchmarks
cargo bench

# Compare benchmarks
cargo bench --bench benchmark_name -- --save-baseline baseline_name
# Make changes
cargo bench --bench benchmark_name -- --baseline baseline_name
```

## Troubleshooting

### Common Issues

#### Build Failures

**Issue**: `error: linking with 'cc' failed`

**Solution**: Install build essentials
```bash
sudo apt-get install build-essential libclang-dev pkg-config
```

**Issue**: `error: could not compile 'openssl-sys'`

**Solution**: Install OpenSSL development libraries
```bash
sudo apt-get install libssl-dev
```

#### Test Failures

**Issue**: Tests fail with timeout

**Solution**: Increase test timeout
```bash
cargo test -- --test-threads=1 --nocapture
```

**Issue**: Integration tests fail

**Solution**: Check that required services are running
```bash
# Start devnet
docker-compose up -d
```

#### Runtime Issues

**Issue**: High memory usage

**Solution**: 
- Check for memory leaks with valgrind
- Review caching strategies
- Adjust configuration limits

**Issue**: Slow performance

**Solution**:
- Enable release mode optimizations
- Profile with perf or flamegraph
- Check database performance

### Getting Help

1. **Documentation**: Check the [docs/](../docs/) directory
2. **Issues**: Search existing [GitHub issues](https://github.com/base/base/issues)
3. **Discussions**: Start a [GitHub discussion](https://github.com/base/base/discussions)
4. **Community**: Join the [Base Discord](https://base.org/discord)

## Additional Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Rust by Example](https://doc.rust-lang.org/rust-by-example/)
- [Reth Documentation](https://reth.rs/)
- [OP Stack Specifications](https://specs.optimism.io/)
- [Base Documentation](https://docs.base.org/)
- [Ethereum Development Documentation](https://ethereum.org/en/developers/docs/)
