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

1. **State Synchronization**: Maintains a cached view of the dispute DAG, anchor pointer, and canonical head used to schedule new work.
2. **Game Creation**: Proposes new games when the finalized L2 head advances past the configured interval.
3. **Game Defense**: Spawns proof-generation tasks for challenged games, reusing the same proving pipeline as fast finality mode when enabled.
4. **Game Resolution & Bonds**: Resolves games the proposer created or proved once eligible and claims credit from finalized wins before pruning them from the cache.

## Configuration

The proposer is configured through environment variables.

Create a `.env.proposer` file in the `fault-proof` directory with all required variables. This single file is used by:
- Docker Compose (for both variable substitution and runtime configuration)
- Direct binary execution (`cargo run --bin proposer` from the `fault-proof` directory; the binary automatically loads `.env.proposer`)

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 RPC endpoint URL |
| `L2_RPC` | L2 RPC endpoint URL |
| `FACTORY_ADDRESS` | Address of the DisputeGameFactory contract |
| `GAME_TYPE` | Type identifier for the dispute game |
| `NETWORK_PRIVATE_KEY` | Private key for the Succinct Prover Network. See the [Succinct Prover Network Quickstart](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart) for setup instructions. (Set to `0x0000000000000000000000000000000000000000000000000000000000000001` if not using fast finality mode) |

For transaction signing, the following methods are supported:

| Method | Description |
|----------|-------------|
| Local wallet | Sign transactions using a private key stored locally |
| Web3 wallet | Sign transactions using an external web3 signer service |
| Google HSM | Sign transactions using Google Cloud Hardware Security Module |

Depending on the one you choose, you must provide the corresponding environment variables:

#### Local wallet

| Variable | Description |
|----------|-------------|
| `PRIVATE_KEY` | Private key for transaction signing (if using private key signer) |

#### Web3 wallet

| Variable | Description |
|----------|-------------|
| `SIGNER_URL` | URL of the web3 signer service (if using web3 signer) |
| `SIGNER_ADDRESS` | Address of the account managed by the web3 signer (if using web3 signer) |

#### Google HSM

| Variable | Description |
|----------|-------------|
| `GOOGLE_PROJECT_ID` | Google Cloud project ID where the HSM key is stored |
| `GOOGLE_LOCATION` | Google Cloud location/region of the key ring (e.g., `us-east1`) |
| `GOOGLE_KEYRING` | Name of the Google Cloud KMS key ring |
| `HSM_KEY_NAME` | Name of the HSM key within the key ring |
| `HSM_KEY_VERSION` | Version number of the HSM key to use |

### Optional Environment Variables

| Variable | Description | Default Value |
|----------|-------------|---------------|
| `L1_CONFIG_DIR` | The directory containing the L1 chain configuration files. | `<project-root>/configs/L1` |
| `L2_CONFIG_DIR` | Directory containing L2 chain configuration files | `<project-root>/configs/L2` |
| `MOCK_MODE` | Whether to use mock mode | `false` |
| `FAST_FINALITY_MODE` | Whether to use fast finality mode | `false` |
| `RANGE_PROOF_STRATEGY` | Proof fulfillment strategy for range proofs. Set to `hosted` to use the hosted proof strategy. | `reserved` |
| `AGG_PROOF_STRATEGY` | Proof fulfillment strategy for aggregation proofs. Set to `hosted` to use the hosted proof strategy. | `reserved` |
| `AGG_PROOF_MODE` | Proof mode for aggregation proofs. Set to `groth16` to use Groth16 proof type. **Note:** Changing the proof mode requires updating the `SP1_VERIFIER` address in `contracts/src/fp/OPSuccinctFaultDisputeGame.sol` to the corresponding verifier gateway contract. See [SP1 Contract Addresses](https://docs.succinct.xyz/docs/sp1/verification/contract-addresses) for verifier addresses. | `plonk` |
| `PROPOSAL_INTERVAL_IN_BLOCKS` | Number of L2 blocks between proposals | `1800` |
| `FETCH_INTERVAL` | Polling interval in seconds | `30` |
| `MAX_CONCURRENT_DEFENSE_TASKS` | Maximum number of concurrently running defense tasks | `8` |
| `L1_BEACON_RPC` | L1 Beacon RPC endpoint URL | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `L2_NODE_RPC` | L2 Node RPC endpoint URL | (Only used if `FAST_FINALITY_MODE` is `true`) |
| `SAFE_DB_FALLBACK` | Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not activated for op-node. When `false`, proposer will return an error if SafeDB is not available. It is by default `false` since using the fallback mechanism will result in higher proving cost. | `false` |
| `PROPOSER_METRICS_PORT` | The port to expose metrics on. Update prometheus.yml to use this port, if using docker compose. | `9000` |
| `FAST_FINALITY_PROVING_LIMIT` | Maximum number of concurrent proving tasks allowed in fast finality mode. | `1` |
| `USE_KMS_REQUESTER` | Whether to expect NETWORK_PRIVATE_KEY to be an AWS KMS key ARN instead of a plaintext private key. | `false` |
| `MAX_PRICE_PER_PGU` | The maximum price per pgu for proving. | `300,000,000` |
| `MIN_AUCTION_PERIOD` | The minimum auction period (in seconds). | `1` |
| `TIMEOUT` | The timeout to use for proving (in seconds). | `14,400` (4 hours) |
| `RANGE_CYCLE_LIMIT` | The cycle limit to use for range proofs. | `1,000,000,000,000` |
| `RANGE_GAS_LIMIT` | The gas limit to use for range proofs. | `1,000,000,000,000` |
| `AGG_CYCLE_LIMIT` | The cycle limit to use for aggregation proofs. | `1,000,000,000,000` |
| `AGG_GAS_LIMIT` | The gas limit to use for aggregation proofs. | `1,000,000,000,000` |
| `WHITELIST` | The list of prover addresses that are allowed to bid on proof requests. | `` |

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
3. Create a `.env.proposer` file with the above configuration
4. Ensure your account has sufficient ETH for bonds and gas

## Running

To run the proposer, from the fault-proof directory:
```bash
# Uses .env.proposer by default
cargo run --bin proposer

# Or specify a custom environment file
cargo run --bin proposer -- --env-file custom.env
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
4. **Task visibility** – Logs the number of active tasks per category so operators can see what work is in-flight.

All long-running work executes in dedicated Tokio tasks stored in a `TaskMap`, preventing duplicate submissions while allowing creation/defense/resolution/bond claiming to progress in parallel. Fast finality mode additionally enforces `FAST_FINALITY_PROVING_LIMIT` before spawning new fast finality proving tasks.

Metrics are published by a separate background collector that samples the canonical head, finalized head, and active proving task count.

## Features

### State Synchronization
- Incrementally loads new games from the factory using a cursor while verifying the output root and parent linkage.
- Refreshes cached status, proposal metadata, deadlines, and credit balances to flag games for resolution or bond claiming.
- Evicts finalized games with no remaining credit and prunes entire subtrees when a parent ends in `CHALLENGER_WINS`.
- Tracks the anchor game and recalculates the canonical head L2 block that drives proposal scheduling.

### Game Creation
- Schedules proposals once the finalized L2 head surpasses `canonical_head + PROPOSAL_INTERVAL_IN_BLOCKS`.
- Computes the expected output root locally and encodes the parent index into the factory call.
- Stakes the factory's initial bond and records the created game's address/index from emitted events.
- Automatically queues fast finality proving for new games when enabled, while gating concurrency via `FAST_FINALITY_PROVING_LIMIT`.
- Supports mock mode to gather proving stats without sending proof requests to the network prover (`MOCK_MODE=true`).

### Game Defense
- Scans cached games for `ProposalStatus::Challenged` entries that remain `IN_PROGRESS`.
- Spawns proof-generation tasks up to `MAX_CONCURRENT_DEFENSE_TASKS`, skipping games that already have an active proving job.
- Uses the same `prove_game` pipeline as fast finality mode, recording instruction cycles and SP1 gas when mock mode is enabled.
- Supports mock mode for testing without using the Succinct Prover Network (`MOCK_MODE=true`).

### Game Resolution
- Flags games for resolution when their proposal status is `Unchallenged` (deadline passed) or a valid proof has been submitted.
- Requires the parent dispute to be resolved and the proposer to be either the creator or prover before submitting a `resolve` transaction.
- Processes eligible games in batches via a dedicated async task and surfaces successes/failures through metrics.

### Bond Claiming
- Flags games for bond claiming once the game registry reports them finalized and there is credit to claim.
- Submits `claimCredit` transactions for the proposer's address.
- Drops games from the cache after bonds are claimed.

### Chain Monitoring
- Recomputes the canonical head by scanning cached games. When an anchor game is present, only its descendants are eligible for canonical head.
- Queries the host/fetcher for the finalized L2 head to decide when creation tasks should trigger.

## Logging

The proposer emits structured `tracing` spans for each job type:
- `[[Proposing]]` wraps game creation attempts and logs `Game created successfully` on success.
- `[[Defending]]` drives challenged-game proving; completed proofs log `Game proven successfully` along with cycle/gas stats in mock mode.
- `[[Proving]]` marks the underlying proof pipeline, while `[[Proposer Resolving]]` and `[[Claiming Proposer Bonds]]` cover resolution and bond recovery.

Adjust verbosity with `RUST_LOG` as needed:

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
- `sync_state`: Keeps the cached dispute DAG aligned with on-chain data (games, anchor, canonical head) and sets resolution/bond flags.
- `handle_completed_tasks`: Cleans up finished asynchronous jobs.
- `spawn_pending_operations`: Ensures there is at most one active task per category while respecting defense/proving concurrency limits.
- `handle_game_creation`: Builds new games once the finalized head crosses the proposal interval and optionally triggers fast finality proving.
- `resolve_games` / `claim_bonds`: Submit on-chain transactions for eligible games and trim settled entries from the cache.
- `run`: Orchestrates the periodic loop, delegating work to the task scheduler.

## Development

When developing or modifying the proposer:
1. Ensure all environment variables are properly set.
2. Test with a local L1/L2 setup first.
3. Monitor logs for proper operation.
4. Test game creation and resolution separately.
5. Verify proper handling of edge cases (network issues, invalid responses, etc.).
6. Test both normal and fast finality modes.
