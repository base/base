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

The proposer is configured through various environment variables. Create a `.env.proposer` file in the fault_proof directory:

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 RPC endpoint URL |
| `L2_RPC` | L2 RPC endpoint URL |
| `FACTORY_ADDRESS` | Address of the DisputeGameFactory contract |
| `GAME_TYPE` | Type identifier for the dispute game |
| `PRIVATE_KEY` | Private key for transaction signing |
| `NETWORK_PRIVATE_KEY` | Private key for the succinct prover network (Set to `0x0000000000000000000000000000000000000000000000000000000000000001` if not using fast finality mode) |

To get a whitelisted key on the Succinct Prover Network for OP Succinct, fill out this [form](https://docs.google.com/forms/d/e/1FAIpQLSd2Yil8TrU54cIuohH1WvDvbxTusyqh5rsDmMAtGC85-Arshg/viewform?ref=https://succinctlabs.github.io/op-succinct/). The Succinct team will reach out to you with an RPC endpoint you can use.

### Optional Environment Variables

| Variable | Description | Default Value |
|----------|-------------|---------------|
| `FAST_FINALITY_MODE` | Whether to use fast finality mode | `false` |
| `PROPOSAL_INTERVAL_IN_BLOCKS` | Number of L2 blocks between proposals | `1800` |
| `FETCH_INTERVAL` | Polling interval in seconds | `30` |
| `ENABLE_GAME_RESOLUTION` | Whether to enable automatic game resolution | `true` |
| `MAX_GAMES_TO_CHECK_FOR_RESOLUTION` | Maximum number of games to check for resolution | `100` |
| `MAX_GAMES_TO_CHECK_FOR_DEFENSE` | Maximum number of recent games to check for defense | `100` |
| `MAX_GAMES_TO_CHECK_FOR_BOND_CLAIMING` | Maximum number of games to check for bond claiming | `100` |
| `L1_BEACON_RPC` | L1 Beacon RPC endpoint URL | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `L2_NODE_RPC` | L2 Node RPC endpoint URL | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `PROVER_ADDRESS` | Address of the account that will be posting output roots to L1. This address is committed to when generating the aggregation proof to prevent front-running attacks. It can be different from the signing address if you want to separate these roles. Default: The address derived from the `PRIVATE_KEY` environment variable. | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `SAFE_DB_FALLBACK` | Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not activated for op-node. When `false`, proposer will return an error if SafeDB is not available. It is by default `false` since using the fallback mechanism will result in higher proving cost. | `false` |
| `PROPOSER_METRICS_PORT` | The port to expose metrics on. Update prometheus.yml to use this port, if using docker compose. | `9000` |

```env
# Required Configuration
L1_RPC=                  # L1 RPC endpoint URL
L2_RPC=                  # L2 RPC endpoint URL
FACTORY_ADDRESS=         # Address of the DisputeGameFactory contract (obtained from deployment)
GAME_TYPE=               # Type identifier for the dispute game (must match factory configuration)
PRIVATE_KEY=             # Private key for transaction signing

# Optional Configuration
FAST_FINALITY_MODE=false                 # Whether to use fast finality mode
PROPOSAL_INTERVAL_IN_BLOCKS=1800         # Number of L2 blocks between proposals
FETCH_INTERVAL=30                        # Polling interval in seconds
ENABLE_GAME_RESOLUTION=false             # Whether to enable automatic game resolution
MAX_GAMES_TO_CHECK_FOR_RESOLUTION=100    # Maximum number of games to check for resolution
MAX_GAMES_TO_CHECK_FOR_DEFENSE=100       # Maximum number of recent games to check for defense
MAX_GAMES_TO_CHECK_FOR_BOND_CLAIMING=100 # Maximum number of games to check for bond claiming
PROPOSER_METRICS_PORT=9000               # The port to expose metrics on
```

### Configuration Steps

1. Deploy the DisputeGameFactory contract following the [deployment guide](./deploy.md)
2. Copy the factory address from the deployment output
3. Create `.env` file with the above configuration
4. Ensure your account has sufficient ETH for bonds and gas

## Running

To run the proposer:
   ```bash
   cargo run --bin proposer
   ```

The proposer will run indefinitely, creating new games and optionally resolving them based on the configuration.

## Features

### Game Creation
- Creates new dispute games at configurable block intervals.
- Computes L2 output roots for game proposals.
- Ensures proper game sequencing with parent-child relationships.
- Handles bond requirements for game creation.
- Supports fast finality mode with proofs. (Set `FAST_FINALITY_MODE=true` in `.env.proposer`)

### Game Defense
- Monitors games for challenges against valid claims
- Automatically defends valid claims by providing proofs
- Checks games within a configurable window (set by `MAX_GAMES_TO_CHECK_FOR_DEFENSE`)
- Only defends games that:
  - Have been challenged
  - Are within their proof submission window
  - Have valid output root claims
- Generates and submits proofs using the Succinct Prover Network

### Game Resolution
When enabled (`ENABLE_GAME_RESOLUTION=true`), the proposer:
- Monitors unchallenged games
- Resolves games after their challenge period expires
- Respects parent-child game relationships in resolution
- Only resolves games whose parent games are already resolved

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
