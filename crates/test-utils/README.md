# Test Utils

A comprehensive integration test framework for node-reth crates.

## Overview

This crate provides reusable testing utilities for integration tests across the node-reth workspace. It includes:

- **Node Setup**: Easy creation of test nodes with Base Sepolia chainspec
- **Engine API Integration**: Control canonical block production and chain advancement
- **Flashblocks Support**: Dummy flashblocks delivery mechanism for testing pending state
- **Test Accounts**: Pre-funded hardcoded accounts (Alice, Bob, Charlie, Deployer)

## Features

### 1. Test Node (`TestNode`)

Create isolated test nodes with Base Sepolia configuration:

```rust
use base_reth_test_utils::TestNode;

#[tokio::test]
async fn test_example() -> eyre::Result<()> {
    // Create a test node with Base Sepolia chainspec and pre-funded accounts
    let node = TestNode::new().await?;

    // Get an alloy provider
    let provider = node.provider().await?;

    // Access test accounts
    let alice = node.alice();
    let balance = node.get_balance(alice.address).await?;

    Ok(())
}
```

**Key Features:**
- Automatic port allocation (enables parallel test execution)
- Disabled P2P discovery (isolated testing)
- Pre-funded test accounts
- HTTP RPC server enabled

### 2. Test Accounts (`TestAccounts`)

Hardcoded test accounts with deterministic addresses and private keys:

```rust
use base_reth_test_utils::TestAccounts;

let accounts = TestAccounts::new();

// Access individual accounts
let alice = &accounts.alice;
let bob = &accounts.bob;
let charlie = &accounts.charlie;
let deployer = &accounts.deployer;

// Each account has:
// - name: Account identifier
// - address: Ethereum address
// - private_key: Private key (hex string)
// - initial_balance_eth: Starting balance in ETH
```

**Account Details:**
- **Alice**: `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` - 10,000 ETH
- **Bob**: `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` - 10,000 ETH
- **Charlie**: `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` - 10,000 ETH
- **Deployer**: `0x90F79bf6EB2c4f870365E785982E1f101E93b906` - 10,000 ETH

These are derived from Anvil's test mnemonic for compatibility.

### 3. Engine API Integration (`EngineContext`)

Control canonical block production via Engine API:

```rust
use base_reth_test_utils::EngineContext;

#[tokio::test]
async fn test_engine_api() -> eyre::Result<()> {
    let node = TestNode::new().await?;

    // Create engine context
    let mut engine = EngineContext::new(
        node.http_url(),
        B256::ZERO, // genesis hash
        1710338135, // initial timestamp
    ).await?;

    // Build and finalize a single canonical block
    let block_hash = engine.build_and_finalize_block().await?;

    // Advance the chain by multiple blocks
    let block_hashes = engine.advance_chain(5).await?;

    // Check current state
    let head = engine.head_hash();
    let block_number = engine.block_number();

    Ok(())
}
```

**Engine Operations:**
- `build_and_finalize_block()` - Create and finalize a single block
- `advance_chain(n)` - Build N blocks sequentially
- `update_forkchoice(...)` - Manual forkchoice updates
- Track current head, block number, and timestamp

### 4. Flashblocks Integration (`FlashblocksContext`)

Dummy flashblocks delivery for testing pending state:

```rust
use base_reth_test_utils::{FlashblocksContext, FlashblockBuilder};

#[tokio::test]
async fn test_flashblocks() -> eyre::Result<()> {
    let (fb_ctx, receiver) = FlashblocksContext::new();

    // Create a base flashblock (first flashblock with base payload)
    let flashblock = FlashblockBuilder::new(1, 0)
        .as_base(B256::ZERO, 1000)
        .with_transaction(tx_bytes, tx_hash, 21000)
        .with_balance(address, U256::from(1000))
        .build();

    // Send flashblock and wait for processing
    fb_ctx.send_flashblock(flashblock).await?;

    // Create a delta flashblock (subsequent flashblock)
    let delta = FlashblockBuilder::new(1, 1)
        .with_transaction(tx_bytes, tx_hash, 21000)
        .build();

    fb_ctx.send_flashblock(delta).await?;

    Ok(())
}
```

**Flashblock Features:**
- Base flashblocks with `ExecutionPayloadBaseV1`
- Delta flashblocks with incremental changes
- Builder pattern for easy construction
- Channel-based delivery (non-WebSocket)

## Architecture

The framework is organized into modules:

```
test-utils/
├── src/
│   ├── lib.rs           # Public API and re-exports
│   ├── accounts.rs      # Test account definitions
│   ├── node.rs          # TestNode implementation
│   ├── engine.rs        # Engine API integration
│   └── flashblocks.rs   # Flashblocks support
├── assets/
│   └── genesis.json     # Base Sepolia genesis configuration
└── Cargo.toml
```

## Usage in Other Crates

Add `base-reth-test-utils` to your `dev-dependencies`:

```toml
[dev-dependencies]
base-reth-test-utils.workspace = true
```

Then use in your integration tests:

```rust
use base_reth_test_utils::{TestNode, TestAccounts};

#[tokio::test]
async fn my_integration_test() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let node = TestNode::new().await?;
    let provider = node.provider().await?;

    // Your test logic here

    Ok(())
}
```

## Design Decisions

1. **Anvil-Compatible Keys**: Uses the same deterministic mnemonic as Anvil for easy compatibility with other tools
2. **Port Allocation**: Random unused ports enable parallel test execution without conflicts
3. **Isolated Nodes**: Disabled P2P discovery ensures tests don't interfere with each other
4. **Channel-Based Flashblocks**: Non-WebSocket delivery mechanism simplifies testing
5. **Builder Patterns**: Fluent APIs for constructing complex test scenarios

## Future Enhancements

This framework is designed to be extended. Planned additions:

- Transaction builders for common operations
- Smart contract deployment helpers
- Snapshot/restore functionality for test state
- Multi-node network simulation
- Performance benchmarking utilities

## Testing

Run the test suite:

```bash
cargo test -p base-reth-test-utils
```

## References

This framework was inspired by:
- [op-rbuilder test framework](https://github.com/flashbots/op-rbuilder/tree/main/crates/op-rbuilder/src/tests/framework)
- [reth e2e-test-utils](https://github.com/paradigmxyz/reth/tree/main/crates/e2e-test-utils)
