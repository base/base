# Fault Proof Proposer

The fault proof proposer is a component responsible for creating and managing OP-Succinct fault dispute games on the L1 chain. It continuously monitors the L2 chain and creates new dispute games at regular intervals to ensure the validity of L2 state transitions.

## Prerequisites

Before running the proposer, ensure you have:

1. Rust toolchain installed (latest stable version)
2. Access to L1 and L2 network nodes
3. The DisputeGameFactory contract deployed (See [Deploy](./deploy.md))
4. Sufficient ETH balance for:
   - Transaction fees
   - Game bonds (configurable in the factory)
5. Required environment variables properly configured (see [Configuration](#configuration))

## Overview

The proposer performs several key functions:

1. **Game Creation**: Creates new dispute games for L2 blocks at configurable intervals
2. **Game Resolution**: Optionally resolves unchallenged games after their deadline passes
3. **Chain Monitoring**: Continuously monitors the L2 chain's safe head and creates proposals accordingly
4. **Fast Finality Mode**: Optionally enables fast finality by including proofs with proposals

## Configuration

The proposer is configured through environment variables.

Create a `.env` file in the `fault-proof` directory with all required variables. This single file is used by:
- Docker Compose (for both variable substitution and runtime configuration)
- Direct binary execution (source it with `. .env` before running)

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 RPC endpoint URL |
| `L2_RPC` | L2 RPC endpoint URL |
| `FACTORY_ADDRESS` | Address of the DisputeGameFactory contract |
| `GAME_TYPE` | Type identifier for the dispute game |
| `NETWORK_PRIVATE_KEY` | Private key for the Succinct Prover Network. See the [Succinct Prover Network Quickstart](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart) for setup instructions. (Set to `0x0000000000000000000000000000000000000000000000000000000000000001` if not using fast finality mode) |

Either `PRIVATE_KEY` or both `SIGNER_URL` and `SIGNER_ADDRESS` must be set for transaction signing:

| Variable | Description |
|----------|-------------|
| `PRIVATE_KEY` | Private key for transaction signing (if using private key signer) |
| `SIGNER_URL` | URL of the web3 signer service (if using web3 signer) |
| `SIGNER_ADDRESS` | Address of the account managed by the web3 signer (if using web3 signer) |

### Optional Environment Variables

| Variable | Description | Default Value |
|----------|-------------|---------------|
| `MOCK_MODE` | Whether to use mock mode | `false` |
| `FAST_FINALITY_MODE` | Whether to use fast finality mode | `false` |
| `RANGE_PROOF_STRATEGY` | Proof fulfillment strategy for range proofs. Set to `hosted` to use the hosted proof strategy. | `reserved` |
| `AGG_PROOF_STRATEGY` | Proof fulfillment strategy for aggregation proofs. Set to `hosted` to use the hosted proof strategy. | `reserved` |
| `PROPOSAL_INTERVAL_IN_BLOCKS` | Number of L2 blocks between proposals | `1800` |
| `FETCH_INTERVAL` | Polling interval in seconds | `30` |
| `MAX_CONCURRENT_DEFENSE_TASKS` | Maximum number of concurrently running defense tasks | `8` |
| `L1_BEACON_RPC` | L1 Beacon RPC endpoint URL | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `L2_NODE_RPC` | L2 Node RPC endpoint URL | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `SAFE_DB_FALLBACK` | Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not activated for op-node. When `false`, proposer will return an error if SafeDB is not available. It is by default `false` since using the fallback mechanism will result in higher proving cost. | `false` |
| `PROPOSER_METRICS_PORT` | The port to expose metrics on. Update prometheus.yml to use this port, if using docker compose. | `9000` |
| `FAST_FINALITY_PROVING_LIMIT` | Maximum number of concurrent proving tasks allowed in fast finality mode. | `1` |

```env
# Required Configuration
L1_RPC=                  # L1 RPC endpoint URL
L2_RPC=                  # L2 RPC endpoint URL
FACTORY_ADDRESS=         # Address of the DisputeGameFactory contract (obtained from deployment)
GAME_TYPE=               # Type identifier for the dispute game (must match factory configuration)

# Transaction Signing Configuration (Choose one)
# Option 1: Private Key Signer
PRIVATE_KEY=             # Private key for transaction signing
# Option 2: Web3 Signer
SIGNER_URL=              # URL of the web3 signer service
SIGNER_ADDRESS=          # Address of the account managed by the web3 signer

# Optional Configuration
MOCK_MODE=false                          # Whether to use mock mode
FAST_FINALITY_MODE=false                 # Whether to use fast finality mode
RANGE_PROOF_STRATEGY=reserved            # Set to hosted to use hosted proof strategy
AGG_PROOF_STRATEGY=reserved              # Set to hosted to use hosted proof strategy
PROPOSAL_INTERVAL_IN_BLOCKS=1800         # Number of L2 blocks between proposals
FETCH_INTERVAL=30                        # Polling interval in seconds
PROPOSER_METRICS_PORT=9000               # The port to expose metrics on
```

### Configuration Steps

1. Deploy the DisputeGameFactory contract following the [deployment guide](./deploy.md)
2. Copy the factory address from the deployment output
3. Create `.env` file with the above configuration
4. Ensure your account has sufficient ETH for bonds and gas

## Running

To run the proposer, from the fault-proof directory:
   ```bash
   cargo run --bin proposer
   ```

The proposer will run indefinitely, creating new games and optionally resolving them based on the configuration.

## Runtime Loop

`OPSuccinctProposer::run` wakes up every `FETCH_INTERVAL` seconds and performs four phases:

1. **State sync** – Reloads dispute games from the factory, tracks the anchor, and recomputes the canonical head using the cached `ProposerState`.
2. **Task cleanup** – Collects results from previously spawned tasks and updates metrics based on success or failure.
3. **Scheduling** – Starts new asynchronous jobs when capacity is available:
   - Game creation once the finalized L2 head surpasses the proposal interval
   - Challenged-game defenses, respecting `MAX_CONCURRENT_DEFENSE_TASKS`
   - Resolution of finished games and bond claims for finalized ones
4. **Metrics refresh** – Publishes gauges such as canonical head block, finalized block, active proving tasks, and error counters.

All long-running work executes in dedicated Tokio tasks stored in a `TaskMap`, preventing duplicate submissions while allowing creation/defense/resolution/bond claiming to progress in parallel. Fast finality mode additionally enforces `FAST_FINALITY_PROVING_LIMIT` before spawning new fast finality proving tasks.

## Features

### Game Creation
- Creates new dispute games at configurable block intervals.
- Computes L2 output roots for game proposals.
- Ensures proper game sequencing with parent-child relationships.
- Handles bond requirements for game creation.
- Supports mock mode for testing without using the Succinct Prover Network. (Set `MOCK_MODE=true` in `.env.proposer`)
- Supports fast finality mode with proofs. (Set `FAST_FINALITY_MODE=true` in `.env.proposer`)

### Game Defense
- Monitors games for challenges against valid claims
- Automatically defends valid claims by providing proofs
- Checks all challenged games tracked in memory
- Only defends games that:
  - Have been challenged
  - Are within their proof submission window
  - Have valid output root claims
- Generates and submits proofs using the Succinct Prover Network
- Supports mock mode for testing without using the Succinct Prover Network. (Set `MOCK_MODE=true` in `.env.proposer`)

### Game Resolution
- Automatically resolves proven games (UnchallengedAndValidProofProvided or ChallengedAndValidProofProvided)
- Monitors unchallenged games and resolves them once their challenge period expires
- Respects parent-child game relationships; a child is only resolved after its parent finishes

### Bond Claiming
- Monitors games for bond claiming opportunities
- Only claims bonds from games that:
  - Are finalized (resolved and airgapped)
  - Has credit left to claim

### Chain Monitoring
- Monitors the L2 chain's finalized (safe) head
- Creates proposals for new blocks as they become available
- Maintains proper spacing between proposals based on configuration
- Tracks the latest valid proposal for proper sequencing

## Logging

The proposer uses the `tracing` crate for logging with a default level of INFO. You can adjust the log level by setting the `RUST_LOG` environment variable:

```bash
RUST_LOG=debug cargo run --bin proposer
```

## Error Handling

The proposer includes robust error handling for:
- RPC connection issues
- Transaction failures
- Contract interaction errors
- Invalid configurations

Errors are logged with appropriate context to aid in debugging.

## Architecture

The proposer is built around the `OPSuccinctProposer` struct which manages:
- Configuration state.
- Wallet management for transactions.
- Game creation, defense, and resolution logic.
- Chain monitoring and interval management.

Key components:
- `ProposerConfig`: Handles environment-based configuration.
- `handle_game_creation`: Main function for proposing new games that:
  - Monitors the L2 chain's safe head.
  - Determines appropriate block numbers for proposals.
  - Creates new games with proper parent-child relationships.
- `handle_game_defense`: Main function for defending challenged games that:
  - Finds the oldest defensible game
  - Generates and submits proofs for valid claims
  - Manages proof generation through the Succinct Prover Network
- `handle_game_resolution`: Main function for resolving games that:
  - Checks if resolution is enabled.
  - Manages resolution of unchallenged games.
  - Respects parent-child relationships.
- `run`: Main loop that:
  - Runs at configurable intervals.
  - Handles game creation, defense, and resolution.
  - Provides error isolation between tasks.

### Helper Functions
- `create_game`: Creates individual games with proper bonding.
- `try_resolve_unchallenged_game`: Attempts to resolve a single game.
- `should_attempt_resolution`: Determines if games can be resolved based on parent status.
- `resolve_unchallenged_games`: Manages batch resolution of games.

## Development

When developing or modifying the proposer:
1. Ensure all environment variables are properly set.
2. Test with a local L1/L2 setup first.
3. Monitor logs for proper operation.
4. Test game creation and resolution separately.
5. Verify proper handling of edge cases (network issues, invalid responses, etc.).
6. Test both normal and fast finality modes.
