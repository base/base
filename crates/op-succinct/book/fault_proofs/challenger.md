# Fault Proof Challenger

The fault proof challenger is a component responsible for monitoring and challenging invalid OP-Succinct fault dispute games on the L1 chain. It continuously scans for invalid games and challenges them to maintain L2 state validity.

## Prerequisites

Before running the challenger, ensure you have:

1. Rust toolchain installed (latest stable version)
2. Access to L1 and L2 network nodes
3. The DisputeGameFactory contract deployed (See [Deploy](./deploy.md))
4. Sufficient ETH balance for:
   - Transaction fees
   - Challenger bonds (proof rewards)
5. Required environment variables properly configured (See [Configuration](#configuration))

## Overview

The challenger performs several key functions:

1. **Game Monitoring**: Continuously syncs new games from the factory, validates their output roots, and marks ones that inherit a challenger win from their parent.
2. **Game Challenging**: Submits challenges for games flagged by the sync step and supports optional malicious testing.
3. **Game Resolution**: Resolves games the challenger countered once their deadlines pass and the parent dispute has settled.
4. **Bond Management**: Tracks finalized games and claims the challenger's credit before removing them from the cache.

## Configuration

The challenger is configured through environment variables. Create a `.env.challenger` file in the project root directory:

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 RPC endpoint URL |
| `L2_RPC` | L2 RPC endpoint URL |
| `FACTORY_ADDRESS` | Address of the DisputeGameFactory contract |
| `GAME_TYPE` | Type identifier for the dispute game |

Either `PRIVATE_KEY` or both `SIGNER_URL` and `SIGNER_ADDRESS` must be set for transaction signing:

| Variable | Description |
|----------|-------------|
| `PRIVATE_KEY` | Private key for transaction signing (if using private key signer) |
| `SIGNER_URL` | URL of the web3 signer service (if using web3 signer) |
| `SIGNER_ADDRESS` | Address of the account managed by the web3 signer (if using web3 signer) |

### Optional Environment Variables

| Variable | Description | Default Value |
|----------|-------------|---------------|
| `FETCH_INTERVAL` | Polling interval in seconds | `30` |
| `CHALLENGER_METRICS_PORT` | The port to expose metrics on. Update prometheus.yml to use this port, if using docker compose. | `9001` |
| `MALICIOUS_CHALLENGE_PERCENTAGE` | Percentage (0.0-100.0) of valid games to challenge for testing defense mechanisms | `0.0` |

```env
# Required Configuration
L1_RPC=                  # L1 RPC endpoint URL
L2_RPC=                  # L2 RPC endpoint URL
FACTORY_ADDRESS=         # Address of the DisputeGameFactory contract
GAME_TYPE=               # Type identifier for the dispute game
PRIVATE_KEY=             # Private key for transaction signing

# Optional Configuration
FETCH_INTERVAL=30                     # Polling interval in seconds
CHALLENGER_METRICS_PORT=9001          # The port to expose metrics on

# Testing Configuration (Optional)
MALICIOUS_CHALLENGE_PERCENTAGE=0.0    # Percentage of valid games to challenge for testing (0.0 = disabled)
```

## Running

To run the challenger:
```bash
cargo run --bin challenger
```

The challenger will run indefinitely, monitoring for invalid games and challenging them as needed.

## Testing Defense Mechanisms

The challenger supports **malicious challenging** of valid games for defense mechanisms testing purposes.

### Configuration

Set `MALICIOUS_CHALLENGE_PERCENTAGE` to enable malicious challenging:

```bash
# Production mode (default) - only challenge invalid games
MALICIOUS_CHALLENGE_PERCENTAGE=0.0

# Testing mode - challenge all valid games
MALICIOUS_CHALLENGE_PERCENTAGE=100.0

# Fine-grained testing - challenge 0.1% of valid games
MALICIOUS_CHALLENGE_PERCENTAGE=0.1

# Mixed testing - challenge 25.5% of valid games
MALICIOUS_CHALLENGE_PERCENTAGE=25.5
```

### Behavior

When malicious challenging is enabled:

1. **Priority 1**: Challenge invalid games (honest challenger behavior)
2. **Priority 2**: Challenge valid games at the configured percentage (defense mechanisms testing behavior); the percentage acts as a per-iteration probability gate.

The challenger will always prioritize challenging invalid games first, then optionally challenge valid games based on the configured percentage.

### Logging

The challenger relies on structured `tracing` logs:
- Honest challenges are logged as `Game challenged successfully` within the `[[Challenging]]` span.
- Malicious attempts emit `\x1b[31m[MALICIOUS CHALLENGE]\x1b[0m` so they stand out in the logs.

## Features

### Game Monitoring
- Incrementally pulls new games from the factory using an on-chain index cursor
- Checks game validity against the L2 state commitment
- Filters to the configured OP Succinct fault dispute game type that was respected at creation time
- Marks games for challenging, resolution, or bond claiming based on proposal status, parent outcomes, and deadlines

### Game Challenging
- Submits challenges for games flagged by the sync step
- Challenges games that are in progress and either invalid or the parent is challenger wins
- Supports malicious challenging of valid games when enabled

### Game Resolution
The challenger:
- Tracks only games it countered
- Resolves games after their deadline once the parent dispute is resolved and it is own game

### Bond Claiming
- Flags challenger-win games once the anchor registry marks them finalized and there is credit to claim
- Claims credit for the challenger's address and removes games from the cache after claiming credit

## Architecture

The challenger is built around the `OPSuccinctChallenger` struct which manages:
- Configuration state
- Wallet management for transactions
- Game challenging and resolution logic
- Chain monitoring and interval management

Key components:
- `ChallengerConfig`: Handles environment-based configuration
- `sync_state`: Keeps the in-memory cache in sync with on-chain state, marking games for challenge, resolution, or bond claims
- `handle_game_challenging`: Submits challenge transactions for games flagged by the sync step and supports malicious testing
- `handle_game_resolution`: Resolves flagged games once they are eligible based on deadlines, parent outcomes and whether it is own game
- `handle_bond_claiming`: Claims challenger credit from finalized games and trims settled entries from the cache
- `run`: Main loop that orchestrates state syncing, challenging, resolution, and bond claiming at the configured interval while isolating task failures

## Error Handling

The challenger includes robust error handling for:
- RPC connection issues
- Transaction failures
- Contract interaction errors
- Invalid configurations

Errors are logged with appropriate context to aid in debugging.

## Development

When developing or modifying the challenger:
1. Ensure all environment variables are properly set
2. Test with a local L1/L2 setup first
3. Monitor logs for proper operation
4. Test challenging and resolution separately
5. Verify proper handling of edge cases
6. Test with various game states and conditions
