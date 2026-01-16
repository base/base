# Base Devnet

A local development network that launches both `base-builder` and `base-client` nodes configured for testing.

## Quick Start

```bash
# Build all binaries
cargo build --release -p base-devnet -p base-builder -p base-reth-node

# Run devnet
./target/release/base-devnet
```

## Features

- **Pre-configured Genesis**: Uses a test genesis with pre-funded accounts
- **Auto-discovery Disabled**: No P2P networking, purely local
- **Deterministic JWT**: Uses a fixed JWT secret for easy scripting
- **Flashblocks Support**: Optional flashblocks mode for testing real-time updates

## CLI Options

```
USAGE:
    base-devnet [OPTIONS]

OPTIONS:
    -d, --datadir <PATH>              Data directory for devnet state
        --http-port <PORT>            Client HTTP RPC port [default: 8545]
        --ws-port <PORT>              Client WebSocket port [default: 8546]
        --engine-port <PORT>          Client Engine API port [default: 8551]
        --builder-http-port <PORT>    Builder HTTP RPC port [default: 8645]
        --builder-engine-port <PORT>  Builder Engine API port [default: 8651]
        --block-time-ms <MS>          Block time in milliseconds [default: 2000]
        --flashblocks                 Enable flashblocks support [default: true]
        --no-builder                  Don't start the builder (client-only)
        --no-client                   Don't start the client (builder-only)
        --no-driver                   Don't start block driver (for external consensus)
    -v, --log-level <LEVEL>           Log level [default: info]
    -h, --help                        Print help
```

## Pre-funded Test Accounts

All accounts have 1,000,000 ETH:

| Name    | Address                                    | Private Key                                                        |
|---------|--------------------------------------------|--------------------------------------------------------------------|
| Alice   | 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 | 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 |
| Bob     | 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 | 0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d |
| Charlie | 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC | 0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a |

## Example Usage

### Basic Development

```bash
# Start devnet with defaults
base-devnet

# In another terminal, interact via cast
cast chain-id --rpc-url http://localhost:8545
cast balance 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545
```

### Custom Configuration

```bash
# Custom ports and persistent data
base-devnet \
    --datadir ./my-devnet \
    --http-port 9545 \
    --block-time-ms 1000

# Client-only mode (for external builder testing)
base-devnet --no-builder

# Builder-only mode
base-devnet --no-client

# External consensus mode (plug in your own op-node/Kona)
base-devnet --no-driver
```

## Architecture

```
┌─────────────────┐         ┌─────────────────┐
│  base-builder   │◄───────►│ base-reth-node  │
│   (block prod)  │ Engine  │    (client)     │
│                 │   API   │                 │
│  :8645 HTTP     │         │  :8545 HTTP     │
│  :8651 Engine   │         │  :8546 WS       │
│  :9002 Metrics  │         │  :8551 Engine   │
│                 │         │  :9001 Metrics  │
└─────────────────┘         └─────────────────┘
```

## Chain Configuration

- **Chain ID**: 84538453
- **Block Gas Limit**: 100,000,000
- **EIP-1559 Elasticity**: 6
- **EIP-1559 Denominator**: 50
- **All Hardforks**: Enabled from genesis (including Prague, Isthmus, Jovian)

## TUI Controls

The devnet includes an interactive terminal UI for monitoring both nodes.

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `r` | Restart nodes |
| `s` | Save logs to file |
| `Tab` | Switch panel focus (Client ↔ Builder) |
| `↑` / `k` | Scroll up |
| `↓` / `j` | Scroll down |
| `PgUp` / `PgDn` | Page up/down |
| `Home` / `g` | Scroll to top |
| `End` / `G` | Scroll to bottom |
| `f` | Toggle auto-scroll |

### Visual Indicators

- **◆ Green line** - Block contains user transaction(s)
- **✓** - Node running
- **✖** - Node stopped
- **Block #N** - Current block number (header)
- **Chain ID** - Network identifier (header)
- **AUTO/MANUAL** - Scroll mode indicator

### Disabling TUI

Run without TUI for CI/scripting:

```bash
cargo build --release -p base-devnet --no-default-features
```

## Troubleshooting

### "Failed to start base-reth-node"

Ensure binaries are built:
```bash
cargo build --release -p base-reth-node -p base-builder
```

### Port already in use

Another devnet or node is running. Kill it or use custom ports:
```bash
base-devnet --http-port 9545 --engine-port 9551
```

### Transactions not being included

1. Check the block driver is running (don't use `--no-driver`)
2. Verify you're using a pre-funded account
3. Check gas price isn't below base fee

### JWT authentication errors

The devnet uses a deterministic all-zeros JWT secret. If connecting external tools, use:
```
0000000000000000000000000000000000000000000000000000000000000000
```

## Testing Transactions

```bash
# Send ETH
cast send 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 \
  --value 0.1ether \
  --rpc-url http://localhost:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

# Check balance
cast balance 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 --rpc-url http://localhost:8545

# Get block info
cast block latest --rpc-url http://localhost:8545
```
