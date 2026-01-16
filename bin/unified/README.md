# `base-unified`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Unified CL+EL binary for Base that runs both consensus and execution layers in a single process.

## Overview

This binary combines:
- **Execution Layer (reth)**: Full OP Stack execution client
- **Consensus Layer (kona)**: Rollup node with derivation pipeline
- **In-Process Bridge**: Zero-overhead communication via direct channels (no HTTP/IPC)

## Architecture

```text
┌────────────────────────────────────────────────────────────────────────┐
│  base-unified binary                                                   │
│  ┌────────────────────────────────────────────────────────────────────┐│
│  │ UnifiedRollupNode (orchestrator)                                   ││
│  │                                                                    ││
│  │  ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐  ││
│  │  │ NetworkActor    │   │ DerivationActor │   │ L1WatcherActor  │  ││
│  │  │ (from kona)     │   │ (from kona)     │   │ (from kona)     │  ││
│  │  └────────┬────────┘   └────────┬────────┘   └────────┬────────┘  ││
│  │           │                     │                     │           ││
│  │           └──────────┬──────────┴──────────┬──────────┘           ││
│  │                      ▼                     ▼                       ││
│  │              mpsc::channel          mpsc::channel                  ││
│  │                      │                     │                       ││
│  │  ┌───────────────────▼─────────────────────▼───────────────────┐  ││
│  │  │ DirectEngineActor (custom implementation)                   │  ││
│  │  │   └─ InProcessEngineDriver (channel-based, no HTTP)         │  ││
│  │  └───────────────────┬─────────────────────────────────────────┘  ││
│  │                      │                                             ││
│  │                      ▼ (direct channel via ConsensusEngineHandle) ││
│  │  ┌─────────────────────────────────────────────────────────────┐  ││
│  │  │ reth EL (EngineApiTreeHandler)                              │  ││
│  │  └─────────────────────────────────────────────────────────────┘  ││
│  └────────────────────────────────────────────────────────────────────┘│
└────────────────────────────────────────────────────────────────────────┘
```

## Usage

```bash
# Run the unified node (launches both EL and CL)
base-unified --chain base-sepolia --rollup-config ./rollup.json

# Run EL only (for testing)
base-unified --chain base-sepolia --el-only
```

## Configuration

### Required Arguments
- `--chain`: Chain specification (e.g., `base-mainnet`, `base-sepolia`)

### Optional Arguments
- `--rollup-config`: Path to kona rollup configuration file
- `--datadir`: Data directory for both CL and EL
- `--l1-rpc-url`: L1 Ethereum RPC endpoint
- `--l1-beacon-url`: L1 Beacon API endpoint
- `--el-only`: Skip consensus layer (EL testing mode)

## Why Unified?

The Engine API (HTTP/JSON-RPC) was designed for Ethereum's multi-client ecosystem where
CL and EL are separate processes. For an L2 rollup where you control both layers:

| Aspect | Separate Processes | Unified Binary |
|--------|-------------------|----------------|
| Latency | HTTP/IPC overhead (~1ms+) | Direct channel (~1μs) |
| Serialization | Full JSON-RPC | None (direct types) |
| Deployment | 2 binaries | 1 binary |
| Coordination | External | Internal |

## Components

This binary uses:
- [`base-engine-bridge`](../../crates/client/engine-bridge): InProcessEngineDriver for channel-based engine communication
- [`base-engine-actor`](../../crates/consensus/engine-actor): DirectEngineActor and processor
- [`base-node-service`](../../crates/consensus/node-service): UnifiedRollupNode orchestrator
- [`base-client-node`](../../crates/client/node): BaseNodeRunner for reth

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
