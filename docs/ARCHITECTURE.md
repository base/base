# Architecture Overview

This document provides a high-level overview of the Base Reth Node architecture, explaining how the different components work together to provide a robust Base L2 network implementation.

## System Architecture

Base Reth Node is built on top of Reth, the Ethereum execution client written in Rust. It extends Reth with Base L2-specific features, particularly Flashblocks and optimized transaction processing.

```
┌─────────────────────────────────────────────────────────────┐
│                     Base Reth Node                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │   Builder    │  │  Consensus   │  │    Client    │     │
│  │              │  │              │  │              │     │
│  │  - Core      │  │  - Protocol  │  │  - Engine    │     │
│  │              │  │  - RPC       │  │  - Flashblocks│    │
│  └──────────────┘  └──────────────┘  │  - Metering  │     │
│                                      │  - Proofs    │     │
│                                      │  - TxPool    │     │
│                                      └──────────────┘     │
│                                                             │
│  ┌───────────────────────────────────────────────────┐     │
│  │              Shared Components                    │     │
│  │                                                   │     │
│  │  - Access Lists  - Flashtypes  - Primitives      │     │
│  │  - Bundles       - JWT         - RPC Types       │     │
│  │  - CLI Utils     - Node        - Engine Ext      │     │
│  └───────────────────────────────────────────────────┘     │
│                                                             │
└─────────────────────────────────────────────────────────────┘
                          │
                          ▼
                  ┌───────────────┐
                  │  Reth Core    │
                  │  (Optimism)   │
                  └───────────────┘
```

## Core Components

### 1. Builder (`crates/builder/`)

The builder component is responsible for block construction and flashblock generation.

**Key Features:**
- Flashblock generation and management
- Transaction ordering and inclusion
- Payload construction for the execution engine
- WebSocket publishing for flashblock updates

**Main Modules:**
- `core/` - Core builder logic and flashblock implementation
- `flashblocks/` - Flashblock-specific functionality including:
  - `generator.rs` - Generates flashblocks at sub-block intervals
  - `payload.rs` - Constructs execution payloads
  - `wspub.rs` - WebSocket publisher for real-time flashblock updates

### 2. Consensus (`crates/consensus/`)

Handles consensus-related operations specific to Base L2.

**Components:**
- `protocol/` - Consensus protocol implementation
- `rpc/` - RPC endpoints for consensus operations

### 3. Client (`crates/client/`)

The main client implementation that integrates all components.

**Key Modules:**

#### Engine (`client/engine/`)
- Manages the execution engine
- Handles payload building and validation
- Coordinates with consensus layer

#### Flashblocks (`client/flashblocks/`)
- Client-side flashblock handling
- State management for preconfirmed transactions
- Integration with RPC layer

#### Metering (`client/metering/`)
- Resource usage tracking
- Transaction simulation for metering
- RPC endpoints for metering data

#### Proofs (`client/proofs/`)
- Proof generation and verification
- Historical proof tracking

#### TxPool (`client/txpool/`)
- Transaction pool management
- Transaction validation and ordering
- Integration with flashblock system

### 4. Shared Components (`crates/shared/`)

Reusable components used across the system.

#### Access Lists (`shared/access-lists/`)
Implements Flashblock-level Access Lists (FAL) as specified in the [access-lists spec](./specs/access-lists.md).

**Purpose:**
- Track all state accesses during flashblock execution
- Enable parallel transaction validation
- Support executionless state updates

**Key Types:**
- `FBALBuilderDb` - Database wrapper for tracking accesses
- `FlashblockAccessListBuilder` - Builder for constructing access lists
- `FlashblockAccessList` - Final access list with RLP encoding

#### Flashtypes (`shared/flashtypes/`)
Core type definitions for flashblock functionality.

#### Primitives (`shared/primitives/`)
Fundamental types and utilities used throughout the codebase.

#### Engine Extensions (`shared/engine-ext/`)
Extensions to the standard engine API for Base-specific functionality.

## Data Flow

### Transaction Processing

```
User Transaction
      │
      ▼
┌──────────┐
│  TxPool  │
└──────────┘
      │
      ▼
┌──────────────┐
│   Builder    │ ──► Flashblock Generation (every ~200ms)
└──────────────┘
      │
      ▼
┌──────────────┐
│   Engine     │ ──► State Execution
└──────────────┘
      │
      ▼
┌──────────────┐
│ Access Lists │ ──► FAL Generation
└──────────────┘
      │
      ▼
┌──────────────┐
│ Flashblocks  │ ──► Preconfirmation
│     RPC      │
└──────────────┘
      │
      ▼
   Clients (via pending tag)
```

### Flashblock Lifecycle

1. **Transaction Arrival**: Transactions enter the mempool via `TxPool`
2. **Flashblock Generation**: Builder creates flashblocks at regular intervals (e.g., 200ms)
3. **State Execution**: Engine executes transactions and updates state
4. **Access List Generation**: FAL tracks all state accesses
5. **Preconfirmation**: Flashblock is published via WebSocket
6. **RPC Availability**: Clients can query preconfirmed state using `"pending"` tag
7. **Canonical Block**: Multiple flashblocks are consolidated into a canonical L2 block

## Key Features

### Flashblocks

Flashblocks are incremental "mini-blocks" produced at sub-block intervals, providing faster transaction preconfirmations.

**Benefits:**
- Reduced latency for transaction confirmation
- Better UX for users and applications
- Maintains security of canonical blocks

**Implementation Details:**
- Flashblocks contain transaction deltas
- Access lists track incremental state changes
- WebSocket publishing for real-time updates

### Flashblock-level Access Lists (FAL)

FAL is an adaptation of EIP-7928 Block-Level Access Lists for flashblock architecture.

**Purpose:**
- Enable parallel disk reads
- Support parallel transaction validation
- Allow executionless state updates

**Key Differences from BAL:**
- Operates on flashblock deltas instead of complete blocks
- Handles OP Stack-specific transaction types
- Records fee vault changes instead of COINBASE

### Metering RPC

Provides resource usage tracking and transaction simulation capabilities.

**Use Cases:**
- Resource accounting for transactions
- Gas estimation improvements
- Performance monitoring

## Configuration

The node can be configured through:

1. **Command-line Arguments**: See `bin/node/src/cli.rs`
2. **Environment Variables**: Defined in `.env.devnet` for development
3. **Configuration Files**: Various TOML files for specific components

## Development Workflow

### Building

```bash
# Standard release build
just build

# Maximum performance build
just build-maxperf

# Direct cargo build
cargo build --release --bin base-reth-node
```

### Testing

```bash
# Run all tests
just test

# Run checks (formatting and clippy)
just check

# Auto-fix issues
just fix

# Full CI suite
just ci
```

### Docker

```bash
# Build client image
docker build -t base-reth-node -f docker/Dockerfile.client .

# Build builder image
docker build -t base-builder -f docker/Dockerfile.builder .

# Run devnet
docker-compose up
```

## Integration Points

### Reth Integration

Base Reth Node builds on Reth's Optimism support:
- Uses Reth's execution engine
- Extends OP Stack transaction types
- Integrates with Reth's database layer

### OP Stack Integration

Specific OP Stack components:
- **L1Block Contract**: Stores L1 block information
- **Fee Vaults**: Sequencer, Base, and L1 fee collection
- **Deposit Transactions**: L1→L2 message passing
- **L1 Attributes Transaction**: Updates L1 block data

### External Interfaces

1. **JSON-RPC**: Standard Ethereum JSON-RPC with flashblock extensions
2. **Engine API**: Communication with consensus layer
3. **WebSocket**: Real-time flashblock updates
4. **Metrics**: Prometheus-compatible metrics endpoints

## Security Considerations

1. **State Validation**: All state transitions are validated
2. **Access Control**: JWT authentication for engine API
3. **Resource Limits**: Configurable limits for transactions and state
4. **Fault Tolerance**: Graceful handling of invalid transactions

## Performance Optimizations

1. **Parallel Processing**: FAL enables parallel transaction validation
2. **Efficient Caching**: Flashblock state caching for quick queries
3. **Database Optimization**: Optimized database access patterns
4. **Memory Management**: Careful memory management for high throughput

## Future Enhancements

Potential areas for future development:
- Enhanced flashblock compression
- Advanced metering capabilities
- Improved proof generation
- Additional RPC methods for flashblock queries

## References

- [Flashblocks Specification](./specs/flashblocks.md)
- [Access Lists Specification](./specs/access-lists.md)
- [Reth Documentation](https://reth.rs/)
- [OP Stack Documentation](https://docs.optimism.io/)
- [Base Documentation](https://docs.base.org/)
