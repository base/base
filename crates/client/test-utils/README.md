# Test Utils

A comprehensive integration test framework for node-reth crates.

## Overview

This crate provides reusable testing utilities for integration tests across the node-reth workspace. It includes:

- **LocalNode**: Isolated in-process node with Base Sepolia chainspec
- **TestHarness**: Unified orchestration layer combining node, Engine API, and flashblocks
- **EngineApi**: Type-safe Engine API client for CL operations
- **Test Accounts**: Pre-funded hardcoded accounts (Alice, Bob, Charlie, Deployer)
- **Flashblocks Support**: Testing pending state with flashblocks delivery

## Quick Start

```rust,ignore
use base_test_utils::TestHarness;

#[tokio::test]
async fn test_example() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;

    // Advance the chain
    harness.advance_chain(5).await?;

    // Access accounts
    let alice = &harness.accounts().alice;

    // Get balance via provider
    let balance = harness.provider().get_balance(alice.address).await?;

    Ok(())
}
```

## Architecture

The framework follows a three-layer architecture:

```text
┌─────────────────────────────────────┐
│         TestHarness                 │  ← Orchestration layer (tests use this)
│  - Coordinates node + engine        │
│  - Builds blocks from transactions  │
│  - Manages test accounts            │
│  - Manages flashblocks              │
└─────────────────────────────────────┘
           │            │
    ┌──────┘            └──────┐
    ▼                          ▼
┌─────────┐              ┌──────────┐
│LocalNode│              │EngineApi │  ← Raw API wrappers
│  (EL)   │              │   (CL)   │
└─────────┘              └──────────┘
```

### Component Responsibilities

- **LocalNode** (EL wrapper): In-process Optimism node with HTTP RPC + Engine API IPC
- **EngineApi** (CL wrapper): Raw Engine API calls (forkchoice, payloads)
- **TestHarness**: Orchestrates block building by fetching latest block headers and calling Engine API

## Components

### 1. TestHarness

The main entry point for integration tests that only need canonical chain control. Combines node, engine, and accounts into a single interface.

```rust,ignore
use base_test_utils::TestHarness;
use alloy_primitives::Bytes;

#[tokio::test]
async fn test_harness() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;

    // Access provider
    let provider = harness.provider();
    let chain_id = provider.get_chain_id().await?;

    // Access accounts
    let alice = &harness.accounts().alice;
    let bob = &harness.accounts().bob;

    // Build empty blocks
    harness.advance_chain(10).await?;

    // Build block with transactions
    let txs: Vec<Bytes> = vec![/* signed transaction bytes */];
    harness.build_block_from_transactions(txs).await?;

    Ok(())
}
```

> Need pending-state testing? Use `FlashblocksHarness` (see Flashblocks section below) to gain `send_flashblock` helpers.

**Key Methods:**
- `new()` - Create new harness with node, engine, and accounts
- `provider()` - Get Alloy RootProvider for RPC calls
- `accounts()` - Access test accounts
- `advance_chain(n)` - Build N empty blocks
- `build_block_from_transactions(txs)` - Build block with specific transactions (auto-prepends the L1 block info deposit)

**Block Building Process:**
1. Fetches latest block header from provider (no local state tracking)
2. Calculates next timestamp (parent + 2 seconds for Base)
3. Calls `engine.update_forkchoice()` with payload attributes
4. Waits for block construction
5. Calls `engine.get_payload()` to retrieve built payload
6. Calls `engine.new_payload()` to validate and submit
7. Calls `engine.update_forkchoice()` again to finalize

### 2. LocalNode

In-process Optimism node with Base Sepolia configuration.

```rust,ignore
use base_test_utils::{LocalNode, default_launcher};

#[tokio::test]
async fn test_node() -> eyre::Result<()> {
    let node = LocalNode::new(default_launcher).await?;

    let provider = node.provider()?;
    let engine = node.engine_api()?;

    Ok(())
}
```

**Features (base):**
- Base Sepolia chain configuration
- Disabled P2P discovery (isolated testing)
- Random unused ports (parallel test safety)
- HTTP RPC server at `node.http_api_addr`
- Engine API IPC at `node.engine_ipc_path`

For flashblocks-enabled nodes, use `FlashblocksLocalNode`:

```rust,ignore
use base_test_utils::FlashblocksLocalNode;

let node = FlashblocksLocalNode::new().await?;
let pending_state = node.flashblocks_state();
node.send_flashblock(flashblock).await?;
```

**Note:** Most tests should use `TestHarness` instead of `LocalNode` directly.

### 3. EngineApi

Type-safe Engine API client wrapping raw CL operations.

```rust,ignore
use base_test_utils::EngineApi;
use alloy_primitives::B256;
use op_alloy_rpc_types_engine::OpPayloadAttributes;

// Usually accessed via TestHarness, but can be used directly
let engine = node.engine_api()?;

// Raw Engine API calls
let fcu = engine.update_forkchoice(current_head, new_head, Some(attrs)).await?;
let payload = engine.get_payload(payload_id).await?;
let status = engine.new_payload(payload, vec![], parent_root, requests).await?;
```

**Methods:**
- `get_payload(payload_id)` - Retrieve built payload by ID
- `new_payload(payload, hashes, root, requests)` - Submit new payload
- `update_forkchoice(current, new, attrs)` - Update forkchoice state

**Note:** EngineApi is stateless. Block building logic lives in `TestHarness`.

### 4. Test Accounts

Hardcoded test accounts with deterministic addresses (Anvil-compatible).

```rust,ignore
use base_test_utils::{TestAccounts, TestHarness};

let accounts = TestAccounts::new();

let alice = &accounts.alice;
let bob = &accounts.bob;
let charlie = &accounts.charlie;
let deployer = &accounts.deployer;

// Access via harness
let harness = TestHarness::new().await?;
let alice = &harness.accounts().alice;
```

**Account Details:**
- **Alice**: `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` - 10,000 ETH
- **Bob**: `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` - 10,000 ETH
- **Charlie**: `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` - 10,000 ETH
- **Deployer**: `0x90F79bf6EB2c4f870365E785982E1f101E93b906` - 10,000 ETH

Each account includes:
- `name` - Account identifier
- `address` - Ethereum address
- `private_key` - Private key (hex string)
- `initial_balance_eth` - Starting balance in ETH

### 5. Flashblocks Support

Use `FlashblocksHarness` when you need `send_flashblock` and access to the in-memory pending state.

```rust,ignore
use base_test_utils::FlashblocksHarness;

#[tokio::test]
async fn test_flashblocks() -> eyre::Result<()> {
    let harness = FlashblocksHarness::new().await?;

    harness.send_flashblock(flashblock).await?;

    let pending = harness.flashblocks_state();
    // assertions...

    Ok(())
}
```

`FlashblocksHarness` derefs to the base `TestHarness`, so you can keep using methods like `provider()`, `build_block_from_transactions`, etc.

Test flashblocks delivery without WebSocket connections by constructing payloads and sending them through `FlashblocksHarness` (or the lower-level `FlashblocksLocalNode`).

## Configuration Constants

Key constants are exported from the crate root:

```rust,ignore
use base_test_utils::{
    BASE_CHAIN_ID,              // Chain ID for Base Sepolia (84532)
    BLOCK_TIME_SECONDS,         // Base L2 block time (2 seconds)
    GAS_LIMIT,                  // Default gas limit (200M)
    NODE_STARTUP_DELAY_MS,      // IPC endpoint initialization (500ms)
    BLOCK_BUILD_DELAY_MS,       // Payload construction wait (100ms)
    L1_BLOCK_INFO_DEPOSIT_TX,   // Pre-captured L1 block info deposit
    L1_BLOCK_INFO_DEPOSIT_TX_HASH,
    DEFAULT_JWT_SECRET,         // All-zeros JWT for local testing
};
```

## File Structure

```text
test-utils/
├── src/
│   ├── lib.rs                 # Public API and re-exports
│   ├── accounts.rs            # Test account definitions
│   ├── constants.rs           # Shared constants (chain ID, timing, etc.)
│   ├── contracts.rs           # Solidity contract bindings
│   ├── engine.rs              # EngineApi (CL wrapper)
│   ├── fixtures.rs            # Genesis loading, provider factories
│   ├── flashblocks_harness.rs # FlashblocksHarness + helpers
│   ├── harness.rs             # TestHarness (orchestration)
│   ├── node.rs                # LocalNode (EL wrapper)
│   └── tracing.rs             # Tracing initialization helpers
├── assets/
│   └── genesis.json           # Base Sepolia genesis
├── contracts/                 # Solidity sources + compiled artifacts
└── Cargo.toml
```

## Usage in Other Crates

Add to `dev-dependencies`:

```toml
[dev-dependencies]
base-test-utils.workspace = true
```

Import in tests:

```rust,ignore
use base_test_utils::TestHarness;

#[tokio::test]
async fn my_test() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    // Your test logic

    Ok(())
}
```

## Design Principles

1. **Separation of Concerns**: LocalNode (EL), EngineApi (CL), TestHarness (orchestration)
2. **Stateless Components**: No local state tracking; always fetch from provider
3. **Type Safety**: Use reth's `OpEngineApiClient` trait instead of raw RPC strings
4. **Parallel Testing**: Random ports + isolated nodes enable concurrent tests
5. **Anvil Compatibility**: Same mnemonic as Anvil for tooling compatibility

## Testing

Run the test suite:

```bash
cargo test -p base-reth-test-utils
```

Run specific test:

```bash
cargo test -p base-reth-test-utils test_harness_setup
```

## Future Enhancements

- Transaction builders for common operations
- Smart contract deployment helpers (Foundry integration planned)
- Snapshot/restore functionality
- Multi-node network simulation
- Performance benchmarking utilities
- Helper builder for Flashblocks

## References

Inspired by:
- [op-rbuilder test framework](https://github.com/flashbots/op-rbuilder/tree/main/crates/op-rbuilder/src/tests/framework)
- [reth e2e-test-utils](https://github.com/paradigmxyz/reth/tree/main/crates/e2e-test-utils)
