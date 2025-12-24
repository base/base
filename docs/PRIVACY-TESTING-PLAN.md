# Privacy Layer Testing Plan for node-reth

> **Purpose**: Comprehensive testing strategy for privacy features including unit tests, integration tests, E2E tests, and manual testing procedures.
>
> **Companion Document**: See PRIVACY-IMPLEMENTATION-PLAN.md for implementation details.

---

## Table of Contents

1. [Testing Overview](#1-testing-overview)
2. [Local Development Environment Setup](#2-local-development-environment-setup)
3. [Unit Tests](#3-unit-tests)
4. [Integration Tests](#4-integration-tests)
5. [End-to-End Tests](#5-end-to-end-tests)
6. [Manual Testing Guide](#6-manual-testing-guide)
7. [Local Block Explorer Setup](#7-local-block-explorer-setup)
8. [Test Contracts & Scenarios](#8-test-contracts--scenarios)
9. [Security Testing](#9-security-testing)
10. [Performance Testing](#10-performance-testing)
11. [CI/CD Integration](#11-cicd-integration)

---

## 1. Testing Overview

### Testing Pyramid

```
                    ┌─────────────┐
                    │   Manual    │  ← Human verification, exploratory testing
                    │   Testing   │
                    └─────────────┘
                   ┌───────────────┐
                   │     E2E       │  ← Full system with real transactions
                   │    Tests      │
                   └───────────────┘
              ┌─────────────────────────┐
              │     Integration         │  ← Multiple components working together
              │       Tests             │
              └─────────────────────────┘
         ┌──────────────────────────────────┐
         │           Unit Tests             │  ← Individual functions/modules
         └──────────────────────────────────┘
```

### Test Categories

| Category | Location | Framework | Purpose |
|----------|----------|-----------|---------|
| Unit Tests | `crates/privacy/src/**/*.rs` | `#[test]` | Test individual functions |
| Integration Tests | `crates/privacy/tests/*.rs` | `#[tokio::test]` | Test component interactions |
| RPC Integration | `crates/rpc/tests/*.rs` | `base-reth-test-utils` | Test RPC filtering |
| E2E Tests | `tests/e2e/*.rs` | Full node harness | Test complete flows |
| Contract Tests | `packages/privacy/test/*.t.sol` | Foundry | Test Solidity library |
| Manual Tests | This document | Human | Exploratory testing |

---

## 2. Local Development Environment Setup

### 2.1 Prerequisites Installation

```bash
# 1. Install Rust (1.88+)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
rustup update

# 2. Install Just (command runner)
# macOS
brew install just
# Linux
cargo install just

# 3. Install Foundry (Solidity toolkit with Anvil, Forge, Cast)
curl -L https://foundry.paradigm.xyz | bash
foundryup

# 4. Install cargo-nextest (fast test runner)
cargo install cargo-nextest

# 5. (Optional) Install Docker for block explorer
# https://docs.docker.com/get-docker/

# 6. Verify installations
just --version
forge --version
anvil --version
cast --version
cargo nextest --version
```

### 2.2 Build node-reth

```bash
# Clone the repository
git clone https://github.com/base/node-reth.git
cd node-reth

# Build release binary
just build

# Verify build
./target/release/base-reth-node --help

# Build test contracts (required for integration tests)
just build-contracts
```

### 2.3 Running the Local Development Node

**Option A: Using the Test Harness (Recommended for Development)**

```bash
# The test harness runs an isolated in-process node
# Use this in your integration tests:
cargo test -p base-reth-test-utils -- --nocapture
```

**Option B: Running the Full Node Binary**

```bash
# Create a data directory
mkdir -p /tmp/node-reth-dev

# Run with development settings
./target/release/base-reth-node \
    --chain base-sepolia \
    --datadir /tmp/node-reth-dev \
    --http \
    --http.api eth,web3,net,debug,trace \
    --http.addr 0.0.0.0 \
    --http.port 8545 \
    --authrpc.addr 0.0.0.0 \
    --authrpc.port 8551 \
    --authrpc.jwtsecret /tmp/jwt.hex \
    --log.file.directory /tmp/node-reth-dev/logs

# Create JWT secret if needed
openssl rand -hex 32 > /tmp/jwt.hex
```

**Option C: Using Anvil for Quick Prototyping**

Anvil is useful for testing Solidity contracts before integrating with node-reth:

```bash
# Start Anvil with 10 funded accounts
anvil

# Anvil will show:
# - RPC URL: http://127.0.0.1:8545
# - 10 accounts with 10,000 ETH each
# - Private keys for all accounts

# Fork Base Sepolia (to test with existing contracts)
anvil --fork-url https://sepolia.base.org

# Anvil with specific chain ID (Base Sepolia = 84532)
anvil --chain-id 84532
```

### 2.4 Deploying Contracts Locally

**Using Foundry (Forge)**

```bash
# Navigate to your contracts directory
cd packages/privacy

# Deploy to local Anvil
forge create src/PrivateERC20.sol:PrivateERC20 \
    --rpc-url http://127.0.0.1:8545 \
    --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
    --constructor-args "Private Token" "PVTK" 1000000000000000000000000

# Deploy to local node-reth (once running)
forge create src/PrivateERC20.sol:PrivateERC20 \
    --rpc-url http://127.0.0.1:8545 \
    --private-key <YOUR_PRIVATE_KEY> \
    --constructor-args "Private Token" "PVTK" 1000000000000000000000000

# Verify deployment
cast call <CONTRACT_ADDRESS> "name()" --rpc-url http://127.0.0.1:8545
```

**Using Forge Scripts**

Create `script/Deploy.s.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/tokens/PrivateERC20.sol";

contract DeployScript is Script {
    function run() external {
        uint256 deployerPrivateKey = vm.envUint("PRIVATE_KEY");
        vm.startBroadcast(deployerPrivateKey);

        PrivateERC20 token = new PrivateERC20(
            "My Private Token",
            "MPT",
            1_000_000 * 10**18
        );

        console.log("Token deployed at:", address(token));

        vm.stopBroadcast();
    }
}
```

Run it:

```bash
PRIVATE_KEY=0xac0974... forge script script/Deploy.s.sol \
    --rpc-url http://127.0.0.1:8545 \
    --broadcast
```

### 2.5 Producing Blocks Locally

**With Anvil:**

Anvil automatically mines blocks when transactions are sent (automining mode). For manual control:

```bash
# Start Anvil with manual mining
anvil --no-mining

# In another terminal, mine blocks manually
cast rpc anvil_mine 1  # Mine 1 block
cast rpc anvil_mine 10 # Mine 10 blocks

# Set automine interval
cast rpc anvil_setIntervalMining 2  # Mine every 2 seconds
```

**With node-reth Test Harness:**

```rust
use base_reth_test_utils::TestHarness;

#[tokio::test]
async fn test_block_production() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;

    // Build empty blocks
    harness.advance_chain(5).await?;

    // Build block with specific transactions
    let tx = create_test_transaction(&harness).await?;
    harness.build_block_from_transactions(vec![tx]).await?;

    Ok(())
}
```

---

## 3. Unit Tests

### 3.1 Privacy Registry Tests

**File:** `crates/privacy/src/registry.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_register_new_contract() {
        let registry = PrivacyRegistry::new();
        let contract = address!("dEAD000000000000000000000000000000000001");

        let config = PrivateContractConfig {
            address: contract,
            admin: address!("dEAD000000000000000000000000000000000002"),
            slots: vec![
                SlotConfig {
                    base_slot: U256::ZERO,
                    slot_type: SlotType::Mapping,
                    ownership: OwnershipType::MappingKey,
                },
            ],
            registered_at: 1,
        };

        assert!(registry.register(config.clone()).is_ok());
        assert!(registry.is_registered(&contract));
    }

    #[test]
    fn test_reject_duplicate_registration() {
        let registry = PrivacyRegistry::new();
        let config = create_test_config();

        registry.register(config.clone()).unwrap();
        let result = registry.register(config);

        assert!(result.is_err());
    }

    #[test]
    fn test_update_slots_by_admin() {
        let registry = PrivacyRegistry::new();
        let admin = address!("dEAD000000000000000000000000000000000002");
        let config = create_test_config_with_admin(admin);

        registry.register(config).unwrap();

        let new_slots = vec![/* ... */];
        let result = registry.update_slots(config.address, new_slots, admin);

        assert!(result.is_ok());
    }

    #[test]
    fn test_reject_update_by_non_admin() {
        let registry = PrivacyRegistry::new();
        let admin = address!("dEAD000000000000000000000000000000000002");
        let attacker = address!("dEAD000000000000000000000000000000000003");
        let config = create_test_config_with_admin(admin);

        registry.register(config).unwrap();

        let new_slots = vec![/* ... */];
        let result = registry.update_slots(config.address, new_slots, attacker);

        assert!(result.is_err());
    }

    #[test]
    fn test_slot_owner_recording() {
        let registry = PrivacyRegistry::new();
        let contract = address!("dEAD000000000000000000000000000000000001");
        let slot = U256::from(12345);
        let owner = address!("dEAD000000000000000000000000000000000099");

        registry.record_slot_owner(contract, slot, owner);

        assert_eq!(registry.get_slot_owner(contract, slot), Some(owner));
    }
}
```

### 3.2 Private Store Tests

**File:** `crates/privacy/src/store.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get_value() {
        let mut store = PrivateStateStore::new();
        let contract = Address::random();
        let slot = U256::from(42);
        let value = U256::from(1000);
        let owner = Address::random();

        store.set(contract, slot, value, owner);

        assert_eq!(store.get(contract, slot), value);
    }

    #[test]
    fn test_get_nonexistent_returns_zero() {
        let store = PrivateStateStore::new();
        let contract = Address::random();
        let slot = U256::from(42);

        assert_eq!(store.get(contract, slot), U256::ZERO);
    }

    #[test]
    fn test_owner_is_authorized() {
        let mut store = PrivateStateStore::new();
        let contract = Address::random();
        let slot = U256::from(42);
        let owner = Address::random();

        store.set(contract, slot, U256::from(100), owner);

        assert!(store.is_authorized(contract, slot, owner, READ));
        assert!(store.is_authorized(contract, slot, owner, WRITE));
    }

    #[test]
    fn test_non_owner_not_authorized_by_default() {
        let mut store = PrivateStateStore::new();
        let contract = Address::random();
        let slot = U256::from(42);
        let owner = Address::random();
        let attacker = Address::random();

        store.set(contract, slot, U256::from(100), owner);

        assert!(!store.is_authorized(contract, slot, attacker, READ));
    }

    #[test]
    fn test_delegate_authorization() {
        let mut store = PrivateStateStore::new();
        let contract = Address::random();
        let slot = U256::from(42);
        let owner = Address::random();
        let delegate = Address::random();

        store.set(contract, slot, U256::from(100), owner);
        store.authorize(contract, slot, delegate, AuthEntry {
            permissions: READ,
            expiry: 0,
            granted_at: 1,
        });

        assert!(store.is_authorized(contract, slot, delegate, READ));
        assert!(!store.is_authorized(contract, slot, delegate, WRITE));
    }

    #[test]
    fn test_authorization_expiry() {
        let mut store = PrivateStateStore::new();
        store.current_block = 100;

        let contract = Address::random();
        let slot = U256::from(42);
        let delegate = Address::random();

        store.authorize(contract, slot, delegate, AuthEntry {
            permissions: READ,
            expiry: 50, // Expired at block 50
            granted_at: 1,
        });

        assert!(!store.is_authorized(contract, slot, delegate, READ));
    }

    #[test]
    fn test_wildcard_authorization() {
        let mut store = PrivateStateStore::new();
        let contract = Address::random();
        let owner = Address::random();
        let delegate = Address::random();

        // Set up some private data
        store.set(contract, U256::from(1), U256::from(100), owner);
        store.set(contract, U256::from(2), U256::from(200), owner);

        // Authorize delegate for ALL slots (slot = 0)
        store.authorize(contract, U256::ZERO, delegate, AuthEntry {
            permissions: READ,
            expiry: 0,
            granted_at: 1,
        });

        assert!(store.is_authorized(contract, U256::from(1), delegate, READ));
        assert!(store.is_authorized(contract, U256::from(2), delegate, READ));
    }

    #[test]
    fn test_revoke_authorization() {
        let mut store = PrivateStateStore::new();
        let contract = Address::random();
        let slot = U256::from(42);
        let delegate = Address::random();

        store.authorize(contract, slot, delegate, AuthEntry {
            permissions: READ,
            expiry: 0,
            granted_at: 1,
        });

        assert!(store.is_authorized(contract, slot, delegate, READ));

        store.revoke(contract, slot, delegate);

        assert!(!store.is_authorized(contract, slot, delegate, READ));
    }
}
```

### 3.3 Slot Classification Tests

**File:** `crates/privacy/src/classification.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unregistered_contract_is_public() {
        let registry = PrivacyRegistry::new();
        let contract = Address::random();
        let slot = U256::from(0);

        let result = classify_slot(&registry, contract, slot);

        assert!(matches!(result, SlotClassification::Public));
    }

    #[test]
    fn test_simple_private_slot() {
        let registry = PrivacyRegistry::new();
        let contract = Address::random();

        registry.register(PrivateContractConfig {
            address: contract,
            admin: Address::ZERO,
            slots: vec![SlotConfig {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            }],
            registered_at: 1,
        }).unwrap();

        let result = classify_slot(&registry, contract, U256::from(5));

        assert!(matches!(result, SlotClassification::Private { owner } if owner == contract));
    }

    #[test]
    fn test_non_registered_slot_is_public() {
        let registry = PrivacyRegistry::new();
        let contract = Address::random();

        registry.register(PrivateContractConfig {
            address: contract,
            admin: Address::ZERO,
            slots: vec![SlotConfig {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            }],
            registered_at: 1,
        }).unwrap();

        // Slot 10 is not registered as private
        let result = classify_slot(&registry, contract, U256::from(10));

        assert!(matches!(result, SlotClassification::Public));
    }

    #[test]
    fn test_mapping_slot_with_recorded_owner() {
        let registry = PrivacyRegistry::new();
        let contract = Address::random();
        let alice = Address::random();

        registry.register(PrivateContractConfig {
            address: contract,
            admin: Address::ZERO,
            slots: vec![SlotConfig {
                base_slot: U256::ZERO, // balances mapping
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            }],
            registered_at: 1,
        }).unwrap();

        // Simulate the computed slot for balances[alice]
        let computed_slot = compute_mapping_slot(U256::ZERO, alice);
        registry.record_slot_owner(contract, computed_slot, alice);

        let result = classify_slot(&registry, contract, computed_slot);

        assert!(matches!(result, SlotClassification::Private { owner } if owner == alice));
    }
}
```

### 3.4 Running Unit Tests

```bash
# Run all privacy crate tests
cargo test -p base-reth-privacy

# Run specific test module
cargo test -p base-reth-privacy registry::tests

# Run with output
cargo test -p base-reth-privacy -- --nocapture

# Run with nextest (faster, better output)
cargo nextest run -p base-reth-privacy
```

---

## 4. Integration Tests

### 4.1 Host Wrapper Integration

**File:** `crates/privacy/tests/host_integration.rs`

```rust
use base_reth_privacy::{PrivacyAwareHost, PrivacyRegistry, PrivateStateStore};
use base_reth_test_utils::TestHarness;

#[tokio::test]
async fn test_sload_routes_private_slot_to_private_store() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let registry = PrivacyRegistry::new();
    let mut private_store = PrivateStateStore::new();

    // Register a contract for privacy
    let contract = Address::random();
    registry.register(PrivateContractConfig {
        address: contract,
        slots: vec![SlotConfig {
            base_slot: U256::from(0),
            slot_type: SlotType::Mapping,
            ownership: OwnershipType::MappingKey,
        }],
        // ...
    })?;

    // Pre-populate private store
    let alice = Address::random();
    let balance_slot = compute_mapping_slot(U256::ZERO, alice);
    private_store.set(contract, balance_slot, U256::from(1000), alice);
    registry.record_slot_owner(contract, balance_slot, alice);

    // Execute SLOAD through privacy-aware host
    let host = PrivacyAwareHost::new(base_host, registry, private_store);
    let result = host.sload(contract, balance_slot);

    assert_eq!(result.unwrap().value, U256::from(1000));

    Ok(())
}

#[tokio::test]
async fn test_sstore_routes_to_correct_backend() -> eyre::Result<()> {
    // Similar test for SSTORE
    Ok(())
}

#[tokio::test]
async fn test_mapping_key_capture_during_sha3() -> eyre::Result<()> {
    // Test that SHA3 opcode intercepts mapping key computation
    Ok(())
}
```

### 4.2 RPC Filtering Integration

**File:** `crates/rpc/tests/privacy_rpc.rs`

```rust
use base_reth_test_utils::TestHarness;
use alloy_primitives::{Address, U256};

#[tokio::test]
async fn test_eth_get_storage_at_returns_zero_for_private_slot() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let provider = harness.provider();

    // Deploy a private ERC20
    let token = deploy_private_erc20(&harness).await?;

    // Transfer tokens to Alice (creates a private balance)
    let alice = harness.accounts().alice.address;
    transfer_tokens(&harness, &token, alice, U256::from(1000)).await?;

    // Try to read Alice's balance slot via eth_getStorageAt
    let balance_slot = compute_erc20_balance_slot(alice);
    let storage_value = provider
        .get_storage_at(token, balance_slot)
        .await?;

    // Should return 0 (privacy filtering)
    assert_eq!(storage_value, U256::ZERO);

    // But balance should be readable via balanceOf() call
    let balance = get_erc20_balance(&harness, &token, alice).await?;
    assert_eq!(balance, U256::from(1000));

    Ok(())
}

#[tokio::test]
async fn test_public_slots_still_readable() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let provider = harness.provider();

    let token = deploy_private_erc20(&harness).await?;

    // totalSupply slot should be public
    let total_supply_slot = U256::from(2); // Depends on contract layout
    let storage_value = provider
        .get_storage_at(token, total_supply_slot)
        .await?;

    // Should return actual value
    assert_ne!(storage_value, U256::ZERO);

    Ok(())
}

#[tokio::test]
async fn test_eth_get_logs_filters_private_events() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let provider = harness.provider();

    // Deploy token with event privacy enabled
    let token = deploy_private_erc20_with_event_filtering(&harness).await?;

    // Do a transfer
    let alice = harness.accounts().alice.address;
    let bob = harness.accounts().bob.address;
    transfer_tokens(&harness, &token, bob, U256::from(500)).await?;

    // Query Transfer events
    let filter = Filter::new().address(token).event("Transfer(address,address,uint256)");
    let logs = provider.get_logs(&filter).await?;

    // Should be empty or filtered
    assert!(logs.is_empty());

    Ok(())
}
```

### 4.3 Precompile Integration

**File:** `crates/privacy/tests/precompile_integration.rs`

```rust
#[tokio::test]
async fn test_privacy_registry_precompile_register() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;

    // Deploy a test contract
    let contract = deploy_test_contract(&harness).await?;

    // Call the privacy registry precompile to register
    let registry_address = address!("0000000000000000000000000000000000000200");

    let calldata = encode_register_call(contract, vec![
        SlotConfig {
            base_slot: U256::ZERO,
            slot_type: SlotType::Mapping,
            ownership: OwnershipType::MappingKey,
        }
    ]);

    let result = harness.provider()
        .call(TransactionRequest {
            to: Some(registry_address),
            data: Some(calldata),
            from: Some(harness.accounts().deployer.address),
            ..Default::default()
        })
        .await?;

    // Verify registration
    let is_registered = call_is_registered(&harness, contract).await?;
    assert!(is_registered);

    Ok(())
}

#[tokio::test]
async fn test_privacy_auth_precompile_authorize() -> eyre::Result<()> {
    // Test the 0x201 authorization precompile
    Ok(())
}
```

### 4.4 Running Integration Tests

```bash
# Run all integration tests
cargo nextest run --workspace --all-features

# Run privacy-specific tests
cargo nextest run -p base-reth-privacy
cargo nextest run -p base-reth-rpc --test privacy_rpc

# Run with verbose output
cargo nextest run --nocapture
```

---

## 5. End-to-End Tests

### 5.1 Full Transaction Flow Test

**File:** `tests/e2e/privacy_e2e.rs`

```rust
use base_reth_test_utils::{TestHarness, FlashblocksHarness};

/// Complete privacy flow: deploy, register, transact, verify privacy
#[tokio::test]
async fn test_complete_privacy_flow() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let alice = harness.accounts().alice;
    let bob = harness.accounts().bob;

    // 1. Deploy PrivateERC20
    let token = deploy_contract(
        &harness,
        "PrivateERC20",
        ("Private USDC", "pUSDC", U256::from(1_000_000e18)),
    ).await?;

    // 2. Verify registration happened in constructor
    let is_registered = harness.provider()
        .call(encode_is_registered(token))
        .await?;
    assert!(decode_bool(is_registered));

    // 3. Build block to finalize deployment
    harness.advance_chain(1).await?;

    // 4. Transfer tokens (creates private balance entries)
    let transfer_tx = create_transfer_tx(&harness, token, alice, bob.address, U256::from(1000)).await?;
    harness.build_block_from_transactions(vec![transfer_tx]).await?;

    // 5. Verify Bob's balance is private
    let bob_balance_slot = compute_erc20_balance_slot(bob.address);
    let storage_query = harness.provider()
        .get_storage_at(token, bob_balance_slot)
        .await?;
    assert_eq!(storage_query, U256::ZERO, "Private slot should return 0");

    // 6. But balanceOf() should work
    let balance = call_balance_of(&harness, token, bob.address).await?;
    assert_eq!(balance, U256::from(1000), "balanceOf should return real value");

    // 7. Verify Alice's balance also private
    let alice_balance_slot = compute_erc20_balance_slot(alice.address);
    let alice_storage = harness.provider()
        .get_storage_at(token, alice_balance_slot)
        .await?;
    assert_eq!(alice_storage, U256::ZERO);

    // 8. Public slot (totalSupply) should be readable
    let total_supply = call_total_supply(&harness, token).await?;
    assert_eq!(total_supply, U256::from(1_000_000e18));

    Ok(())
}

/// Test authorization delegation flow
#[tokio::test]
async fn test_authorization_delegation_flow() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let alice = harness.accounts().alice;
    let auditor = harness.accounts().charlie;

    // 1. Deploy and transfer to Alice
    let token = deploy_private_erc20(&harness).await?;
    transfer_to_alice(&harness, token).await?;

    // 2. Alice authorizes auditor to read her balance
    let auth_tx = create_authorize_tx(
        &harness,
        token,
        alice,
        auditor.address,
        U256::ZERO, // all slots
        READ,
        0, // no expiry
    ).await?;
    harness.build_block_from_transactions(vec![auth_tx]).await?;

    // 3. Verify auditor can now query (via authenticated RPC - future)
    // For now, verify authorization was recorded
    let is_authorized = call_is_authorized(&harness, token, U256::ZERO, auditor.address).await?;
    assert!(is_authorized);

    Ok(())
}

/// Test with flashblocks (pending state)
#[tokio::test]
async fn test_privacy_with_flashblocks() -> eyre::Result<()> {
    let harness = FlashblocksHarness::new().await?;

    // Deploy and register
    let token = deploy_private_erc20(&harness).await?;

    // Send flashblock with transfer
    let transfer_tx = create_transfer(&harness).await?;
    harness.send_flashblock(create_flashblock(vec![transfer_tx])).await?;

    // Query pending state - balance should still be private
    let balance_slot = compute_erc20_balance_slot(harness.accounts().bob.address);
    let pending_storage = harness.provider()
        .get_storage_at(token, balance_slot)
        .block_id(BlockId::pending())
        .await?;

    assert_eq!(pending_storage, U256::ZERO, "Private slots hidden even in pending");

    Ok(())
}
```

### 5.2 Running E2E Tests

```bash
# Run all E2E tests
cargo nextest run -p e2e-tests

# Run with specific test
cargo nextest run test_complete_privacy_flow

# Run with logging
RUST_LOG=debug cargo nextest run -p e2e-tests
```

---

## 6. Manual Testing Guide

### 6.1 Quick Start Checklist

```
[ ] Build node-reth: `just build`
[ ] Start local node or Anvil
[ ] Deploy test contract
[ ] Interact and verify privacy
[ ] (Optional) Run block explorer
```

### 6.2 Step-by-Step Manual Test

**Step 1: Start Local Environment**

```bash
# Terminal 1: Start Anvil (simpler for initial testing)
anvil --chain-id 84532

# OR start node-reth
./target/release/base-reth-node --chain base-sepolia --http
```

**Step 2: Deploy a Private ERC20**

```bash
# Navigate to privacy contracts
cd packages/privacy

# Deploy using Forge
forge create src/tokens/PrivateERC20.sol:PrivateERC20 \
    --rpc-url http://127.0.0.1:8545 \
    --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
    --constructor-args "Test Private Token" "TPT" 1000000000000000000000000

# Note the deployed address from output
# Example: Deployed to: 0x5FbDB2315678afecb367f032d93F642f64180aa3
```

**Step 3: Interact with the Token**

```bash
export TOKEN=0x5FbDB2315678afecb367f032d93F642f64180aa3
export ALICE=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
export BOB=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
export ALICE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

# Check Alice's balance (via balanceOf)
cast call $TOKEN "balanceOf(address)" $ALICE --rpc-url http://127.0.0.1:8545

# Transfer tokens to Bob
cast send $TOKEN "transfer(address,uint256)" $BOB 1000000000000000000000 \
    --rpc-url http://127.0.0.1:8545 \
    --private-key $ALICE_KEY

# Check Bob's balance (via balanceOf)
cast call $TOKEN "balanceOf(address)" $BOB --rpc-url http://127.0.0.1:8545
```

**Step 4: Verify Privacy**

```bash
# Compute Bob's balance slot
# For standard ERC20, balances are at slot 0
# balances[bob] = keccak256(abi.encodePacked(bob, uint256(0)))

# Use cast to compute the slot
BOB_SLOT=$(cast keccak "$(cast abi-encode "f(address,uint256)" $BOB 0)")
echo "Bob's balance slot: $BOB_SLOT"

# Try to read Bob's balance via eth_getStorageAt
cast storage $TOKEN $BOB_SLOT --rpc-url http://127.0.0.1:8545

# Expected output with privacy: 0x0 (zero)
# Without privacy: would show actual balance

# Verify via balanceOf still works
cast call $TOKEN "balanceOf(address)" $BOB --rpc-url http://127.0.0.1:8545
# Should show: 1000000000000000000000 (1000 tokens)
```

**Step 5: Test Authorization (once implemented)**

```bash
export AUTH_PRECOMPILE=0x0000000000000000000000000000000000000201
export AUDITOR=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC

# Alice authorizes auditor to read her balance
# authorize(address contract_, bytes32 slot, address delegate, uint8 permissions, uint256 expiry)
cast send $AUTH_PRECOMPILE \
    "authorize(address,bytes32,address,uint8,uint256)" \
    $TOKEN \
    0x0000000000000000000000000000000000000000000000000000000000000000 \
    $AUDITOR \
    1 \
    0 \
    --rpc-url http://127.0.0.1:8545 \
    --private-key $ALICE_KEY
```

### 6.3 Common Manual Test Scenarios

| Scenario | Steps | Expected Result |
|----------|-------|-----------------|
| Deploy private token | `forge create PrivateERC20 ...` | Contract deploys, is auto-registered |
| Check registration | `cast call $REGISTRY "isRegistered(address)" $TOKEN` | Returns `true` |
| Transfer tokens | `cast send $TOKEN "transfer(address,uint256)" ...` | TX succeeds |
| Query balance slot | `cast storage $TOKEN $SLOT` | Returns `0x0` (private) |
| Query balanceOf | `cast call $TOKEN "balanceOf(address)" ...` | Returns real balance |
| Query totalSupply | `cast call $TOKEN "totalSupply()"` | Returns real supply |

---

## 7. Local Block Explorer Setup

### 7.1 Otterscan (Recommended for Development)

[Otterscan](https://github.com/otterscan/otterscan) is a lightweight block explorer that works well with local development.

**Anvil has built-in Otterscan API support!**

```bash
# Terminal 1: Start Anvil (already has Otterscan API)
anvil

# Terminal 2: Run Otterscan frontend
docker run --rm -p 5100:80 \
    --name otterscan \
    -e ERIGON_URL="http://host.docker.internal:8545" \
    otterscan/otterscan:v2.6.0

# Open browser: http://localhost:5100
```

For node-reth, you'll need to ensure the Otterscan API is exposed (check if this is implemented).

### 7.2 Blockscout (Full-Featured)

[Blockscout](https://github.com/blockscout/blockscout) is more resource-intensive but provides comprehensive features.

```bash
# Clone Blockscout
git clone https://github.com/blockscout/blockscout.git
cd blockscout/docker-compose

# Create .env file
cat > envs/common-blockscout.env << 'EOF'
ETHEREUM_JSONRPC_HTTP_URL=http://host.docker.internal:8545
ETHEREUM_JSONRPC_TRACE_URL=http://host.docker.internal:8545
ETHEREUM_JSONRPC_WS_URL=ws://host.docker.internal:8545
CHAIN_ID=84532
COIN=ETH
EOF

# Start Blockscout
docker-compose -f docker-compose.yml up -d

# Access at http://localhost:4000
```

### 7.3 Foundry's Builtin Tools

Foundry provides simpler inspection tools:

```bash
# View transaction details
cast tx <TX_HASH> --rpc-url http://127.0.0.1:8545

# View receipt
cast receipt <TX_HASH> --rpc-url http://127.0.0.1:8545

# Decode transaction input
cast 4byte-decode <CALLDATA>

# Get block info
cast block latest --rpc-url http://127.0.0.1:8545

# Trace a transaction (requires trace API)
cast run <TX_HASH> --rpc-url http://127.0.0.1:8545
```

### 7.4 Privacy Verification via Block Explorer

When testing privacy, verify in the block explorer that:

1. **Transactions are visible**: Transfer txs should appear normally
2. **Contract state is hidden**: Storage reads via explorer should show `0x0` for private slots
3. **Events may be filtered**: Depending on event privacy config
4. **balanceOf() works**: Contract read functions return real values

---

## 8. Test Contracts & Scenarios

### 8.1 Test Contract: SimplePrivateStorage

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {PrivacyEnabled, OwnershipType} from "@base/privacy/PrivacyEnabled.sol";

/// @notice Simple contract for testing private storage
contract SimplePrivateStorage is PrivacyEnabled {
    // Slot 0: Private (contract-owned)
    uint256 private secretValue;

    // Slot 1: Public
    uint256 public publicValue;

    // Slot 2: Private mapping (key-owned)
    mapping(address => uint256) private userSecrets;

    constructor() {
        _privateSlot(0);  // secretValue
        _privateMapping(2, OwnershipType.MappingKey);  // userSecrets
        _finalizePrivacy();

        secretValue = 42;
        publicValue = 100;
    }

    function setSecret(uint256 value) external {
        secretValue = value;
    }

    function getSecret() external view returns (uint256) {
        return secretValue;
    }

    function setUserSecret(uint256 value) external {
        userSecrets[msg.sender] = value;
    }

    function getUserSecret(address user) external view returns (uint256) {
        return userSecrets[user];
    }
}
```

### 8.2 Test Contract: PrivateERC20 with Events

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {PrivacyEnabled, OwnershipType} from "@base/privacy/PrivacyEnabled.sol";

/// @notice ERC20 with private balances and allowances
contract TestPrivateERC20 is ERC20, PrivacyEnabled {
    constructor(
        string memory name_,
        string memory symbol_,
        uint256 initialSupply
    ) ERC20(name_, symbol_) {
        _mint(msg.sender, initialSupply);
        _registerAsERC20();
    }

    // Override transfer to verify it still works with privacy
    function transfer(address to, uint256 amount) public override returns (bool) {
        return super.transfer(to, amount);
    }
}
```

### 8.3 Forge Test Suite

**File:** `packages/privacy/test/PrivateERC20.t.sol`

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/tokens/PrivateERC20.sol";

contract PrivateERC20Test is Test {
    PrivateERC20 public token;
    address public alice = address(0x1);
    address public bob = address(0x2);

    function setUp() public {
        token = new PrivateERC20("Test", "TST", 1_000_000 ether);
    }

    function testDeploy() public {
        assertEq(token.name(), "Test");
        assertEq(token.symbol(), "TST");
        assertEq(token.totalSupply(), 1_000_000 ether);
    }

    function testTransfer() public {
        token.transfer(alice, 1000 ether);
        assertEq(token.balanceOf(alice), 1000 ether);
    }

    function testBalanceOfReturnsCorrectValue() public {
        token.transfer(alice, 500 ether);
        token.transfer(bob, 300 ether);

        assertEq(token.balanceOf(alice), 500 ether);
        assertEq(token.balanceOf(bob), 300 ether);
    }

    function testApproveAndTransferFrom() public {
        token.transfer(alice, 1000 ether);

        vm.prank(alice);
        token.approve(bob, 500 ether);

        vm.prank(bob);
        token.transferFrom(alice, bob, 500 ether);

        assertEq(token.balanceOf(bob), 500 ether);
        assertEq(token.balanceOf(alice), 500 ether);
    }
}
```

**Run Forge tests:**

```bash
cd packages/privacy
forge test -vvv
```

---

## 9. Security Testing

### 9.1 Privacy Leak Detection

```rust
/// Verify no private data leaks via any RPC method
#[tokio::test]
async fn test_no_privacy_leaks_via_rpc() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;
    let token = deploy_private_erc20(&harness).await?;

    // Transfer to create private balance
    let alice = harness.accounts().alice.address;
    transfer_tokens(&harness, token, alice, U256::from(1000)).await?;

    // Test each RPC method that could leak data

    // 1. eth_getStorageAt
    let slot = compute_balance_slot(alice);
    let storage = harness.provider().get_storage_at(token, slot).await?;
    assert_eq!(storage, U256::ZERO, "eth_getStorageAt leaks private data");

    // 2. eth_getProof (storage proof)
    let proof = harness.provider().get_proof(token, vec![slot]).await?;
    assert!(proof.storage_proof[0].value.is_zero(), "eth_getProof leaks data");

    // 3. debug_traceTransaction (if enabled)
    // Should not include private storage values in trace

    // 4. eth_getLogs
    // Filter events as configured

    Ok(())
}
```

### 9.2 Authorization Boundary Tests

```rust
#[tokio::test]
async fn test_cannot_read_others_private_data() -> eyre::Result<()> {
    // Attacker cannot read Alice's balance via any method
}

#[tokio::test]
async fn test_cannot_modify_others_private_data() -> eyre::Result<()> {
    // Attacker cannot write to Alice's private slots
}

#[tokio::test]
async fn test_expired_authorization_rejected() -> eyre::Result<()> {
    // Verify expired auth entries are properly rejected
}
```

### 9.3 Fuzzing

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn fuzz_slot_classification_never_panics(
        contract in any::<[u8; 20]>(),
        slot in any::<[u8; 32]>()
    ) {
        let registry = PrivacyRegistry::new();
        let contract = Address::from_slice(&contract);
        let slot = U256::from_be_bytes(slot);

        // Should never panic, always return valid classification
        let _ = classify_slot(&registry, contract, slot);
    }

    #[test]
    fn fuzz_private_store_operations(
        contract in any::<[u8; 20]>(),
        slot in any::<[u8; 32]>(),
        value in any::<[u8; 32]>(),
    ) {
        let mut store = PrivateStateStore::new();
        let contract = Address::from_slice(&contract);
        let slot = U256::from_be_bytes(slot);
        let value = U256::from_be_bytes(value);
        let owner = Address::random();

        store.set(contract, slot, value, owner);
        assert_eq!(store.get(contract, slot), value);
    }
}
```

---

## 10. Performance Testing

### 10.1 Benchmark: Slot Classification

**File:** `crates/privacy/benches/classification.rs`

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use base_reth_privacy::*;

fn bench_classify_slot(c: &mut Criterion) {
    let registry = PrivacyRegistry::new();
    let contract = Address::random();

    // Register a contract with multiple slot configs
    registry.register(PrivateContractConfig {
        address: contract,
        slots: (0..10).map(|i| SlotConfig {
            base_slot: U256::from(i),
            slot_type: SlotType::Mapping,
            ownership: OwnershipType::MappingKey,
        }).collect(),
        // ...
    }).unwrap();

    c.bench_function("classify_slot_registered", |b| {
        b.iter(|| {
            classify_slot(black_box(&registry), black_box(contract), black_box(U256::from(5)))
        })
    });

    c.bench_function("classify_slot_unregistered", |b| {
        let random_contract = Address::random();
        b.iter(|| {
            classify_slot(black_box(&registry), black_box(random_contract), black_box(U256::from(5)))
        })
    });
}

criterion_group!(benches, bench_classify_slot);
criterion_main!(benches);
```

### 10.2 Benchmark: Private Store Operations

```rust
fn bench_private_store(c: &mut Criterion) {
    let mut store = PrivateStateStore::new();
    let contract = Address::random();

    // Pre-populate with 10000 entries
    for i in 0..10000 {
        store.set(contract, U256::from(i), U256::from(i * 100), Address::random());
    }

    c.bench_function("private_store_get", |b| {
        b.iter(|| {
            store.get(black_box(contract), black_box(U256::from(5000)))
        })
    });

    c.bench_function("private_store_set", |b| {
        b.iter(|| {
            store.set(
                black_box(contract),
                black_box(U256::from(10001)),
                black_box(U256::from(42)),
                black_box(Address::random())
            )
        })
    });
}
```

### 10.3 Running Benchmarks

```bash
# Run privacy benchmarks
cargo bench -p base-reth-privacy

# Compare before/after optimization
cargo bench -p base-reth-privacy -- --save-baseline before
# Make changes...
cargo bench -p base-reth-privacy -- --baseline before
```

---

## 11. CI/CD Integration

### 11.1 GitHub Actions Workflow

**File:** `.github/workflows/privacy-tests.yml`

```yaml
name: Privacy Tests

on:
  push:
    paths:
      - 'crates/privacy/**'
      - 'packages/privacy/**'
  pull_request:
    paths:
      - 'crates/privacy/**'
      - 'packages/privacy/**'

jobs:
  rust-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install nextest
        uses: taiki-e/install-action@nextest

      - name: Run privacy unit tests
        run: cargo nextest run -p base-reth-privacy

      - name: Run RPC integration tests
        run: cargo nextest run -p base-reth-rpc --test privacy_rpc

  solidity-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Foundry
        uses: foundry-rs/foundry-toolchain@v1

      - name: Run Forge tests
        working-directory: packages/privacy
        run: forge test -vvv

  e2e-tests:
    runs-on: ubuntu-latest
    needs: [rust-tests, solidity-tests]
    steps:
      - uses: actions/checkout@v4

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Build node
        run: cargo build --release

      - name: Run E2E tests
        run: cargo nextest run -p e2e-tests
```

### 11.2 Pre-commit Hooks

```bash
# .git/hooks/pre-commit
#!/bin/bash

# Run privacy tests before commit
cargo test -p base-reth-privacy --quiet
if [ $? -ne 0 ]; then
    echo "Privacy tests failed. Commit aborted."
    exit 1
fi

# Check Solidity formatting
cd packages/privacy && forge fmt --check
```

---

## Quick Reference Commands

```bash
# Build everything
just build && just build-contracts

# Run all tests
just test

# Run privacy-specific tests
cargo nextest run -p base-reth-privacy
cargo nextest run -p base-reth-rpc --test privacy_rpc

# Run Solidity tests
cd packages/privacy && forge test -vvv

# Start local Anvil
anvil --chain-id 84532

# Deploy contract
forge create src/PrivateERC20.sol:PrivateERC20 \
    --rpc-url http://127.0.0.1:8545 \
    --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
    --constructor-args "Token" "TKN" 1000000000000000000000000

# Query storage slot
cast storage <CONTRACT> <SLOT> --rpc-url http://127.0.0.1:8545

# Call contract function
cast call <CONTRACT> "balanceOf(address)" <ADDRESS> --rpc-url http://127.0.0.1:8545

# Start Otterscan explorer
docker run --rm -p 5100:80 -e ERIGON_URL="http://host.docker.internal:8545" otterscan/otterscan:v2.6.0
```

---

## Sources

- [Foundry Documentation](https://getfoundry.sh/anvil/overview/)
- [Otterscan GitHub](https://github.com/otterscan/otterscan)
- [Blockscout GitHub](https://github.com/blockscout/blockscout)
- [Base Test Networks Documentation](https://docs.base.org/learn/deployment-to-testnet/test-networks)
- [QuickNode Foundry Guide](https://www.quicknode.com/guides/ethereum-development/smart-contracts/intro-to-foundry)

---

**End of Testing Plan**
