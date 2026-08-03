# Standalone proving

This workflow lets an operator discover an existing L1 dispute game, purchase a
PLONK proof from the Succinct Prover Network using a locally run prover stack,
and submit the completed proof to the game on L1. It does not start a local L1
or L2 devnet.

The proof is matched to the game's on-chain block range, L1 head, intermediate
output roots, and submitting wallet. A proof generated for one game or wallet
cannot be reused for another.

## What runs locally

`just prover up <network>` starts three containers from
[`docker-compose.prover.yml`](../../etc/docker/docker-compose.prover.yml):

| Service | Local role | Exposure and storage |
| --- | --- | --- |
| `prover-service-postgres` | Stores requests, leases, status, and proof results. | Internal only. Data persists under `.zk-prover/<network>/postgres`. |
| `base-prover-service` | Queues requester jobs and leases them to workers. | Requester JSON-RPC is exposed at `http://127.0.0.1:9000` by default. Worker RPC on port 9001 remains inside Compose. |
| `base-prover-zk-host` | Claims jobs, generates witnesses, calls the selected proving backend, and returns results. | Internal only. Connects outbound to the configured chain RPCs and Succinct Network. |

`basectl` runs on the host. It talks to L1 directly for game reads and proof
submission, and to the local prover-service requester RPC for proof jobs. The
standalone stack does not run a Base node, Ethereum node, or local SP1 prover;
you provide RPC endpoints and the worker purchases proof generation from the
Succinct Prover Network.

## Data flow

```text
basectl
  |-- read game state -------------------------------> L1 execution RPC
  |-- enqueue/status/fetch proof --> prover-service --> Postgres
                                            ^
                                            | claim, heartbeat, result
                                            |
                                         zk-host
                                            |-- L1 headers/blobs --> L1 execution + beacon RPCs
                                            |-- L2 blocks/state ---> Base execution + consensus RPCs
                                            `-- compressed range proof + PLONK aggregation
                                                --> Succinct Prover Network

basectl <-- serialized SP1 PLONK receipt <-- prover-service
  `-- verifyProposalProof(encoded proof) ------------> dispute game on L1
```

The worker first generates the witness from the four chain RPCs. For a proposal
proof it submits a compressed range-proof request to Succinct, then submits a
PLONK aggregation request after the range proof completes. The worker returns
the serialized SP1 receipt to prover-service, which stores it with the session
in Postgres. `basectl proofs submit` fetches that receipt, converts it to the
contract proof encoding, and sends it as calldata to
`AggregateVerifier.verifyProposalProof` on the dispute-game proxy.

Two different funded keys are involved:

- `NETWORK_PRIVATE_KEY` is the Succinct requester key used by `zk-host`. It pays
  for proving in PROVE.
- `BASECTL_SUBMITTER_PRIVATE_KEY` is an L1 wallet key used only by `basectl
  proofs submit`. It pays L1 gas, and its address is committed into the proof
  when the proof is proposed.

## Prerequisites

- Docker with Buildx and Compose.
- Rust and `just` for building the Base binaries and SP1 programs.
- L1 execution and beacon RPC endpoints.
- Base execution and consensus/rollup RPC endpoints for the same network.
- A funded Succinct requester key with enough PROVE for both proving stages.
- An L1 submitter wallet with enough ETH for the final transaction.
- The target network's `DisputeGameFactory` address.

The RPCs must provide historical data for the game's range. If an RPC runs on
the Docker host, use `http://host.docker.internal:<port>` in the worker
environment; `localhost` inside the container refers to the container itself.

## Start the stack

Build the real SP1 ELFs once. Paid network proving refuses to start with stub
ELFs:

```sh
just succinct build-elfs
```

Export endpoints for one network. These values are consumed by the container,
so host-local endpoints use `host.docker.internal`:

```sh
export L1_NODE_ADDRESS=https://your-l1-execution-rpc.example
export L1_BEACON_ADDRESS=https://your-l1-beacon-rpc.example
export L2_NODE_ADDRESS=https://your-base-execution-rpc.example
export BASE_CONSENSUS_ADDRESS=https://your-base-consensus-rpc.example
export NETWORK_PRIVATE_KEY=0xYOUR_FUNDED_SUCCINCT_REQUESTER_KEY

just prover up sepolia
```

The network label scopes the Compose project, worker ID, and Postgres data. Use
a distinct label for each chain or endpoint set.

Inspect the stack with:

```sh
just prover logs sepolia

docker compose -p base-prover-sepolia \
  -f etc/docker/docker-compose.prover.yml ps
```

Set a different host requester port with `PROVER_SERVICE_RPC_PORT` before
starting the stack. The examples below use the default:

```sh
export BASECTL_PROVER_RPC=http://127.0.0.1:9000
```

## Discover a game

Built-in mainnet and Sepolia basectl presets do not hardcode a dispute-game
factory. Pass it explicitly or add `proofs.dispute_game_factory` to a custom
network YAML.

```sh
export DISPUTE_GAME_FACTORY=0xYOUR_FACTORY_ADDRESS

cargo run -p basectl -- -c sepolia proofs games \
  --factory "$DISPUTE_GAME_FACTORY" \
  --missing-zk
```

Inspect one result before spending proving funds:

```sh
export GAME_ADDRESS=0xYOUR_GAME_ADDRESS

cargo run -p basectl -- -c sepolia proofs games "$GAME_ADDRESS" \
  --factory "$DISPUTE_GAME_FACTORY"
```

`proofs games` reads L1 only; the local prover stack is not required for these
commands.

## Request the proposal proof

Choose the L1 wallet that will later submit the proof. The proof journal commits
to this address, so the final transaction must come from the same wallet.

```sh
export BASECTL_SUBMITTER_PRIVATE_KEY=0xYOUR_L1_SUBMITTER_KEY
export PROVER_ADDRESS="$(cast wallet address \
  --private-key "$BASECTL_SUBMITTER_PRIVATE_KEY")"

cargo run -p basectl -- -c sepolia proofs propose "$GAME_ADDRESS" \
  --factory "$DISPUTE_GAME_FACTORY" \
  --prover-address "$PROVER_ADDRESS" \
  --zk-backend network \
  --wait
```

Before enqueueing, basectl reads the game and refuses games that are no longer
in progress, already contain a ZK proof, or have an invalid block range. It
copies the block range, pinned L1 head, and intermediate-root interval from the
game into the request.

Without `--session-id`, basectl derives a deterministic ID from the network,
backend, game, block range, and prover address. Re-running the same request uses
the existing session instead of purchasing a duplicate proof.

The request can run for hours. It is safe to omit `--wait` and inspect it later:

```sh
cargo run -p basectl -- -c sepolia proofs list
cargo run -p basectl -- -c sepolia proofs status <SESSION_ID>
just prover logs sepolia base-prover-zk-host
```

## Submit the completed proof

When the session succeeds, fetch and submit the proof with the same wallet:

```sh
cargo run -p basectl -- -c sepolia proofs submit "$GAME_ADDRESS" \
  --factory "$DISPUTE_GAME_FACTORY" \
  --private-key "$BASECTL_SUBMITTER_PRIVATE_KEY" \
  --zk-backend network \
  --wait
```

Prefer the `BASECTL_SUBMITTER_PRIVATE_KEY` environment variable over writing the
key directly in shell history:

```sh
cargo run -p basectl -- -c sepolia proofs submit "$GAME_ADDRESS" \
  --factory "$DISPUTE_GAME_FACTORY" \
  --wait
```

When no explicit session ID is provided, `submit` derives the same ID from the
signing wallet. Before sending, it re-reads the game and refuses to spend gas if
the game is no longer in progress or already has a ZK proof. It then waits for
one L1 confirmation and prints the transaction hash, block number, and gas used.

## Stop or reset

Stop one network-scoped stack while preserving its database:

```sh
just prover down sepolia
```

To permanently remove its local proof history after stopping it, delete only
that network's directory:

```sh
rm -rf .zk-prover/sepolia
```

Do not expose ports 9000 or 9001 publicly. The prover-service requester and
worker APIs are unauthenticated and are intended for trusted local networking.
Treat both private keys and the persisted proof database as sensitive operator
data.
