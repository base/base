# node-reth Privacy Layer Technical Specification

> **Purpose**: Reference for implementing private storage in Base's node-reth sequencer.
>
> **Audience**: node-reth developers (primary), smart contract developers (secondary).

**Version:** 5.0 | **Status:** Draft | **Last Updated:** 2025-12-23

---

## Table of Contents

**Part 1: Core Concepts**
1. [Introduction](#1-introduction)
2. [Privacy Model](#2-privacy-model)
3. [Developer Philosophy](#3-developer-philosophy)

**Part 2: Transaction Model**
4. [Two Endpoints](#4-two-endpoints)
5. [Privacy Modes](#5-privacy-modes)
6. [Private Transaction Flow](#6-private-transaction-flow)

**Part 3: Infrastructure**
7. [Precompiles](#7-precompiles)
8. [State Persistence](#8-state-persistence)
9. [RPC Layer](#9-rpc-layer)
10. [Exit Mechanism](#10-exit-mechanism)

**Part 4: Contract Patterns**
11. [PrivateERC20](#11-privateerc20)
12. [DualBalanceERC20](#12-dualbalanceerc20)
13. [PrivacyVault](#13-privacyvault)
14. [Anonymous Voting](#14-anonymous-voting)

**Part 5: Implementation**
15. [Implementation Phases](#15-implementation-phases)
16. [File Locations](#16-file-locations)

**Appendices**
- [Appendix A: Precompile Reference](#appendix-a-precompile-reference)
- [Glossary](#glossary)

---

# Part 1: Core Concepts

---

## 1. Introduction

### The Problem

All smart contract storage on Ethereum and L2s is publicly visible. Anyone can:
- See every user's token balance via `eth_getStorageAt`
- Track wallet movements and DeFi positions
- Front-run transactions based on pending state
- Link on-chain activity to real identities

### The Solution

**Private storage slots.** Developers mark specific storage slots as private. These slots are stored separately from the public state trie. The contract code doesn't change—only where the data lives.

```
┌─────────────────────────────────────────────────────────────┐
│                       CORE INSIGHT                          │
│                                                             │
│   Same contract bytecode, two storage backends:             │
│                                                             │
│   • Public slots  → State trie (visible to everyone)        │
│   • Private slots → Private store (visible only to owner)   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    node-reth Sequencer                       │
│                                                             │
│   ┌─────────────────────────────────────────────────────┐   │
│   │  Two Transaction Endpoints                           │   │
│   │  ├─ eth_sendRawTransaction  (standard, in blocks)   │   │
│   │  └─ priv_sendRawTransaction (privacy-controlled)    │   │
│   └─────────────────────────────────────────────────────┘   │
│                           │                                  │
│                           ▼                                  │
│   ┌─────────────────────────────────────────────────────┐   │
│   │  Execution Engine                                    │   │
│   │  ├─ Storage routing (private/public)                │   │
│   │  └─ Mode handling (Real/Shielded)                   │   │
│   └─────────────────────────────────────────────────────┘   │
│                    │                │                        │
│                    ▼                ▼                        │
│   ┌──────────────────────┐  ┌──────────────────────┐        │
│   │  State Trie          │  │  Private Store       │        │
│   └──────────────────────┘  └──────────────────────┘        │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Key Principles

| Principle | Description |
|-----------|-------------|
| **Two endpoints** | `eth_*` for standard, `priv_*` for privacy-controlled |
| **Two modes** | Real (identity visible) or Shielded (identity hidden) |
| **Honest semantics** | msg.sender always matches on-chain sender |
| **User controls linkability** | Shielded index=0 for persistent, index>0 for unlinkable |

---

## 2. Privacy Model

### 2.1 Slot Classification

Every storage slot is classified as **Public** or **Private** at contract registration:

```
┌─────────────────────────────────────────────────────────────┐
│  Example: Voting Contract                                   │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  PUBLIC SLOTS                                               │
│  ├─ yesCount                                                │
│  └─ noCount                                                 │
│                                                             │
│  PRIVATE SLOTS                                              │
│  └─ hasVoted mapping                                        │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### 2.2 Slot Types

```rust
enum SlotType {
    Simple,                          // Single value
    Mapping,                         // mapping(K => V)
    NestedMapping,                   // mapping(K1 => mapping(K2 => V))
    MappingToStruct { struct_size: u8 },
}
```

### 2.3 Ownership Types

Who can read private slots via RPC:

```rust
enum OwnershipType {
    Contract,    // Only during execution
    MappingKey,  // balances[alice] → alice owns
    OuterKey,    // allowances[owner][spender] → owner owns
    InnerKey,    // delegations[from][to] → to owns
}
```

---

## 3. Developer Philosophy

### 3.1 Contracts Use msg.sender

Contracts work normally. `msg.sender` is always the address that appears on-chain:

```solidity
function vote(bool yes) external {
    // msg.sender is either:
    // - Real user (if Real mode)
    // - Shielded address (if Shielded mode)
    require(!_hasVoted[msg.sender], "Already voted");
    _hasVoted[msg.sender] = true;

    if (yes) yesCount++;
    else noCount++;
}
```

### 3.2 No Precompile Needed

Unlike previous designs, contracts don't need special precompiles to identify users. The privacy mode determines `msg.sender`:

| Mode | msg.sender | On-chain "from" |
|------|-----------|-----------------|
| Real | Alice | Alice |
| Shielded | 0x9876... | 0x9876... |

**What you see is what you get.** The sender during execution matches the sender on-chain.

### 3.3 When to Use What

| Scenario | Endpoint | Mode | msg.sender |
|----------|----------|------|------------|
| Normal tx, don't care about privacy | `eth_*` | N/A | User |
| Private reads + public identity | `priv_*` | Real | User |
| Pure private, no public writes | `priv_*` | Either | Depends |
| Anonymous swap (unlinkable) | `priv_*` | Shielded(idx>0) | Fresh addr |
| DeFi position (persistent) | `priv_*` | Shielded(idx=0) | Same addr |

---

# Part 2: Transaction Model

---

## 4. Two Endpoints

### 4.1 eth_sendRawTransaction

Standard Ethereum transaction:
- Always included in block
- User's identity visible
- Storage routed based on slot classification
- Use when: you don't need to hide your identity

### 4.2 priv_sendRawTransaction

Private transaction with privacy mode:
- May or may not appear in block (depends on storage access)
- User specifies mode: `Real` or `Shielded { protocol, index }`
- Use when: you want privacy control

```rust
struct PrivateTransaction {
    from: Address,           // Real user (for nonce, signature)
    to: Address,
    data: Bytes,
    value: U256,             // Must be 0 (ETH transfers not supported)
    gas_limit: u64,
    private_nonce: u64,
    mode: PrivacyMode,
    chain_id: u64,
    signature: Signature,
}

enum PrivacyMode {
    /// msg.sender = real user. On-chain tx from real user.
    /// Use when: identity reveal is OK (poker reveal, public actions)
    Real,

    /// msg.sender = derived shielded address. On-chain tx from shielded.
    /// index=0: persistent per protocol (DeFi positions consolidate)
    /// index>0: fresh/unlinkable (anonymous swaps, votes)
    Shielded { protocol: Address, index: u64 },
}
```

---

## 5. Privacy Modes

### 5.1 Real Mode

**Rule:** msg.sender = real user. If public writes occur, real user's tx appears in block.

```
User submits priv_* with mode = Real
          │
          ▼
Execute as user (msg.sender = Alice)
          │
          ├── Only private writes? ──▶ Commit, done (zero footprint)
          │
          └── Any public writes? ────▶ Alice's tx appears in block
```

**Use case:** Poker reveal, any action where identity disclosure is acceptable.

### 5.2 Shielded Mode

**Rule:** msg.sender = derived shielded address. If public writes occur, shielded address's tx appears in block.

```
User submits priv_* with mode = Shielded { protocol, index }
          │
          ▼
Derive shielded address from (seed, protocol, chain_id, index)
          │
          ▼
Execute as shielded (msg.sender = 0x9876...)
          │
          ├── Only private writes? ──▶ Commit, done (zero footprint)
          │
          └── Any public writes? ────▶ Shielded tx appears in block
```

**Properties:**
- Each user has unique shielded addresses derived from their seed
- `index=0`: Same address per protocol (positions consolidate)
- `index>0`: Fresh addresses (unlinkable between actions)
- Shielded address is a real EOA, sequencer holds derived key

**Use case:** Anonymous swaps (index>0), DeFi positions (index=0).

### 5.3 Shielded Address Derivation

```rust
fn derive_shielded(
    user_seed: [u8; 32],
    protocol: Address,
    chain_id: u64,
    index: u64
) -> (Address, PrivateKey) {
    let derived = keccak256(user_seed || protocol || chain_id || index || "shielded");
    let privkey = PrivateKey::from(derived);
    (privkey.to_address(), privkey)
}
```

**User controls linkability:**

| Use case | Protocol | Index | Result |
|----------|----------|-------|--------|
| Aerodrome position | Aerodrome | 0 | Same address, positions consolidate |
| Anonymous swap #1 | Aerodrome | 1 | Fresh address |
| Anonymous swap #2 | Aerodrome | 2 | Different fresh address |
| Morpho lending | Morpho | 0 | Persistent for Morpho |

### 5.4 Comparison

| Mode | msg.sender | On-chain "from" | Use Case |
|------|-----------|-----------------|----------|
| Real | Alice | Alice | Poker reveal, identity OK |
| Shielded(idx=0) | 0x9876 | 0x9876 | DeFi positions |
| Shielded(idx>0) | 0xABC1 | 0xABC1 | Anonymous swaps |

---

## 6. Private Transaction Flow

### 6.1 Submission

```
┌────────────────────────────────────────────────────────────┐
│  priv_sendRawTransaction                                    │
│                                                             │
│  1. Validate:                                               │
│     - private_nonce matches stored nonce (by real user)    │
│     - signature is valid (signed by real user)             │
│     - value == 0 (no ETH transfers via priv_*)             │
│                                                             │
│  2. Determine msg.sender:                                   │
│     - Real mode: msg.sender = from (real user)             │
│     - Shielded: msg.sender = derive_shielded(...)          │
│                                                             │
│  3. Execute transaction                                     │
│                                                             │
│  4. Check storage access:                                   │
│     - Any SSTORE to public slot?                            │
│                                                             │
│  5. Handle based on storage access:                         │
│     (see matrix below)                                      │
│                                                             │
│  6. Increment private nonce (by real user address)         │
│                                                             │
│  7. Return receipt                                          │
└────────────────────────────────────────────────────────────┘
```

### 6.2 Decision Matrix

| Mode | Only Private Writes | Has Public Writes |
|------|--------------------|--------------------|
| Real | Commit, no block tx | Real user tx in block |
| Shielded | Commit, no block tx | Shielded tx in block |

**Note:** "Only private writes" = zero on-chain footprint for both modes.

### 6.3 Example: Anonymous Swap

```solidity
contract PrivacyVault {
    mapping(address => uint256) private _balances;  // PRIVATE

    function swap(address tokenOut, uint256 amountIn) external {
        require(_balances[msg.sender] >= amountIn);
        _balances[msg.sender] -= amountIn;  // Private write

        // Public write to Uniswap
        underlying.approve(UNISWAP, amountIn);
        IUniswap(UNISWAP).swap(...);
    }
}
```

**Flow:**
```
1. Alice submits: priv_sendRawTransaction({
     to: PrivacyVault,
     data: swap(USDC, 1000),
     mode: Shielded { protocol: PrivacyVault, index: 42 }
   })

2. Derive shielded address: 0xABC1... (fresh, index=42)

3. Execute:
   - msg.sender = 0xABC1
   - _balances[0xABC1] -= 1000 (private write)
   - Uniswap swap (public writes)

4. Public writes detected + Shielded mode:
   - Sequencer signs tx FROM 0xABC1
   - Includes in block

5. On-chain: "0xABC1 called PrivacyVault.swap"
   - No link to Alice
   - Index 42 is fresh, won't be reused
```

### 6.4 Private Nonce

Replay protection for priv_* transactions:

```rust
// Keyed by REAL user address, not shielded
table!(PrivateNonces);  // Address → u64
```

- Separate from public Ethereum nonce
- Keyed by real user (Alice), not shielded address
- Incremented for ALL successful priv_* (regardless of mode)
- Not visible on chain

### 6.5 Gas

| Mode | Who Pays | How |
|------|----------|-----|
| Real (no block tx) | Sequencer sponsors | Rate-limited |
| Real (block tx) | Real user EOA | User funds it |
| Shielded (no block tx) | Sequencer sponsors | Rate-limited |
| Shielded (block tx) | Shielded EOA | Sequencer funds from user's deposit |

**Shielded gas funding:**
- Sequencer maintains mapping of real user → deposited ETH
- When shielded tx needs gas, sequencer funds shielded address
- Details TBD for v1

---

# Part 3: Infrastructure

---

## 7. Precompiles

### 7.1 Privacy Registry (0x0200)

Registers contracts for privacy:

```solidity
interface IPrivacyRegistry {
    function register(
        address admin,
        SlotConfig[] calldata slots,
        bool hideEvents
    ) external;

    function isRegistered(address contract_) external view returns (bool);
}
```

### 7.2 Privacy Auth (0x0201)

Manages read/write authorization:

```solidity
interface IPrivacyAuth {
    function grant(
        address contract_,
        uint256 slot,
        address delegate,
        uint8 permissions
    ) external;

    function revoke(address contract_, uint256 slot, address delegate) external;

    function isAuthorized(
        address contract_,
        uint256 slot,
        address delegate
    ) external view returns (uint8 permissions);
}
```

---

## 8. State Persistence

### 8.1 MDBX Tables

```rust
table!(PrivateStorage);   // (contract, slot) → value
table!(PrivateNonces);    // real_address → u64
table!(PrivateAuth);      // (contract, slot, delegate) → permissions
table!(ShieldedSeeds);    // real_address → encrypted seed
```

### 8.2 Seed Management

```rust
fn get_or_create_seed(user: Address) -> [u8; 32] {
    if let Some(encrypted) = ShieldedSeeds::get(user) {
        decrypt(encrypted)
    } else {
        let seed = random_bytes(32);
        ShieldedSeeds::insert(user, encrypt(seed));
        seed
    }
}
```

**Note:** Seed creation happens on first Shielded mode usage.

---

## 9. RPC Layer

### 9.1 Modified Endpoints

| Endpoint | Modification |
|----------|--------------|
| `eth_getStorageAt` | Return 0 for unauthorized private access |
| `eth_getLogs` | Filter if `hideEvents=true` |
| `eth_call` | Route storage based on slot classification |

### 9.2 New priv_* Endpoints

| Endpoint | Description |
|----------|-------------|
| `priv_sendRawTransaction` | Submit private tx with mode |
| `priv_getPrivateNonce` | Get current private nonce |
| `priv_getStorageAt` | Get private storage (with auth) |
| `priv_getShieldedAddress` | Get shielded address for protocol+index |

---

## 10. Exit Mechanism

### 10.1 Purpose

Users can exit if sequencer fails.

### 10.2 Approach

Sequencer publishes merkle roots. Contracts implement force-exit:

```solidity
interface IPrivacyExitable {
    function forceExit(
        address user,
        uint256 amount,
        bytes32[] calldata proof
    ) external;
}
```

---

# Part 4: Contract Patterns

---

## 11. PrivateERC20

All balances private:

```solidity
contract PrivateERC20 is ERC20, PrivacyEnabled {
    constructor(string memory name_, string memory symbol_)
        ERC20(name_, symbol_)
    {
        _registerPrivateMapping(0);  // _balances
        _registerPrivateAllowances(1);
        _finalizePrivacyRegistration();
    }

    // Use priv_* with Real mode for zero-footprint transfers
    // Use eth_* if you want transfer events public
}
```

---

## 12. DualBalanceERC20

Public and private balances:

```solidity
contract DualBalanceERC20 is ERC20, PrivacyEnabled {
    mapping(address => uint256) private _privateBalances;

    constructor(string memory name_, string memory symbol_)
        ERC20(name_, symbol_)
    {
        _registerPrivateMapping(5);  // _privateBalances
        _finalizePrivacyRegistration();
    }

    function shield(uint256 amount) external {
        _burn(msg.sender, amount);
        _privateBalances[msg.sender] += amount;
    }

    function unshield(uint256 amount) external {
        _privateBalances[msg.sender] -= amount;
        _mint(msg.sender, amount);
    }

    function privateTransfer(address to, uint256 amount) external {
        // msg.sender is either real user or shielded address
        require(_privateBalances[msg.sender] >= amount);
        _privateBalances[msg.sender] -= amount;
        _privateBalances[to] += amount;
    }
}
```

---

## 13. PrivacyVault

Wrap existing tokens with privacy + anonymous swaps:

```solidity
contract PrivacyVault is PrivacyEnabled {
    IERC20 public immutable underlying;
    mapping(address => uint256) private _balances;

    constructor(IERC20 _underlying) {
        underlying = _underlying;
        _registerPrivateMapping(0);
        _finalizePrivacyRegistration();
    }

    function deposit(uint256 amount) external {
        underlying.transferFrom(msg.sender, address(this), amount);
        _balances[msg.sender] += amount;
    }

    function withdraw(uint256 amount, address to) external {
        // msg.sender is real user or shielded address
        require(_balances[msg.sender] >= amount);
        _balances[msg.sender] -= amount;
        underlying.transfer(to, amount);
    }

    function privateTransfer(address to, uint256 amount) external {
        require(_balances[msg.sender] >= amount);
        _balances[msg.sender] -= amount;
        _balances[to] += amount;
    }

    /// Swap via Uniswap - use Shielded mode for anonymity
    function swap(
        address tokenOut,
        uint256 amountIn,
        uint256 minOut,
        address recipient
    ) external {
        require(_balances[msg.sender] >= amountIn);
        _balances[msg.sender] -= amountIn;

        // Vault is msg.sender to Uniswap
        underlying.approve(UNISWAP, amountIn);
        uint256 received = IUniswap(UNISWAP).swap(
            address(underlying),
            tokenOut,
            amountIn,
            minOut,
            recipient
        );

        // If recipient is this vault, credit the caller
        if (recipient == address(this)) {
            _balances[msg.sender] += received;
        }
    }
}
```

**Usage:**
- `deposit()` via `eth_*` (public deposit)
- `privateTransfer()` via `priv_* Real` (zero footprint)
- `swap()` via `priv_* Shielded(idx>0)` (anonymous swap)

---

## 14. Anonymous Voting

```solidity
contract AnonymousVoting is PrivacyEnabled {
    mapping(address => bool) private _hasVoted;  // PRIVATE
    uint256 public yesCount;
    uint256 public noCount;
    uint256 public deadline;

    constructor(uint256 _deadline) {
        deadline = _deadline;
        _registerPrivateMapping(0);  // _hasVoted
        _finalizePrivacyRegistration();
    }

    function vote(bool yes) external {
        require(block.timestamp < deadline, "Voting ended");

        // msg.sender is shielded address when using Shielded mode
        require(!_hasVoted[msg.sender], "Already voted");
        _hasVoted[msg.sender] = true;  // Private write

        if (yes) yesCount++;      // Public write
        else noCount++;
    }

    function hasVoted(address user) external view returns (bool) {
        return _hasVoted[user];  // Only visible to user (via auth)
    }
}
```

**Usage:** Call `vote()` via `priv_* Shielded { protocol: Voting, index: 0 }`
- Public tally updated
- On-chain shows shielded address voted
- No link to real identity

**Limitation:** Same user could vote again with different index. For strict one-per-person voting, use Real mode (identity revealed) or implement registration.

---

# Part 5: Implementation

---

## 15. Implementation Phases

### Phase 1: State Persistence
- [ ] MDBX tables
- [ ] Private nonce tracking (by real address)
- [ ] Crash recovery

### Phase 2: Basic priv_* with Real Mode
- [ ] PrivateTransaction struct
- [ ] PrivacyMode enum
- [ ] Nonce validation
- [ ] Storage routing

### Phase 3: Shielded Mode
- [ ] Seed generation and storage
- [ ] Shielded address derivation with index
- [ ] Shielded tx creation and signing
- [ ] Gas funding for shielded addresses

### Phase 4: Developer Experience
- [ ] Contract templates
- [ ] Testing tools
- [ ] Documentation

---

## 16. File Locations

### New/Modified Files

| File | Purpose |
|------|---------|
| `crates/privacy/src/mode.rs` | PrivacyMode enum |
| `crates/privacy/src/shielded.rs` | Shielded address derivation |
| `crates/privacy/src/transaction.rs` | PrivateTransaction handling |

### Contract Files

| File | Purpose |
|------|---------|
| `contracts/examples/PrivacyVault.sol` | Example vault |
| `contracts/examples/AnonymousVoting.sol` | Example voting |

---

# Appendices

---

## Appendix A: Precompile Reference

### Privacy Registry (0x0200)

| Function | Selector | Gas |
|----------|----------|-----|
| `register(...)` | `0x4bb056b3` | 5000 + 2000/slot |
| `addSlots(...)` | `0x50ad0d4c` | 5000 + 2000/slot |
| `isRegistered(address)` | `0xc3c5a547` | 100 |

### Privacy Auth (0x0201)

| Function | Selector | Gas |
|----------|----------|-----|
| `grant(...)` | `0x758e42eb` | 3000 |
| `revoke(...)` | `0x92f5f34e` | 3000 |
| `isAuthorized(...)` | `0xe87752e9` | 100 |

---

## Glossary

| Term | Definition |
|------|------------|
| **Private Slot** | Storage routed to private store |
| **priv_*** | RPC for privacy-controlled transactions |
| **PrivacyMode** | Real or Shielded |
| **Real Mode** | msg.sender = real user, identity visible |
| **Shielded Mode** | msg.sender = derived address, identity hidden |
| **Shielded Address** | Per-user derived EOA for anonymous actions |
| **Shielded Index** | 0 for persistent, >0 for fresh/unlinkable |
| **Zero Footprint** | Transaction not included in any block |

---

## Summary

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│  eth_sendRawTransaction                                     │
│  • Standard tx, always in block                             │
│  • Identity visible                                         │
│  • Use when: you don't need privacy                         │
│                                                             │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  priv_sendRawTransaction + Real mode                        │
│  • msg.sender = real user                                   │
│  • If public writes: real user tx in block                  │
│  • If only private: zero footprint                          │
│  • Use when: identity reveal is OK                          │
│                                                             │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  priv_sendRawTransaction + Shielded mode (index=0)          │
│  • msg.sender = persistent shielded address per protocol    │
│  • Positions consolidate under one address                  │
│  • Use when: stateful DeFi (lending, LP)                    │
│                                                             │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  priv_sendRawTransaction + Shielded mode (index>0)          │
│  • msg.sender = fresh shielded address                      │
│  • Each action unlinkable                                   │
│  • Use when: anonymous one-off actions (swaps, votes)       │
│                                                             │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Contracts just use msg.sender normally.                    │
│  No special precompiles needed for identity.                │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

---

**node-reth Privacy Layer Technical Specification v5.0**
