# `basectl`

`basectl` is the operator console for Base infrastructure. It combines interactive terminal
dashboards with scriptable commands to inspect blocks, node sync, peers, transaction pools,
Flashblocks, data availability, pods, upgrades, and proofs; diagnose node health; and safely
operate HA conductor and sequencer clusters across mainnet, Sepolia, and local devnets.

If you run, debug, or automate Base infrastructure, this README documents the RPC access each command needs, human and JSON output modes, configuration and discovery behavior, and the confirmation and partial-failure semantics of state-changing operations.

## Usage

```
basectl [OPTIONS] [COMMAND]
```

Global options:

| Flag                    | Default   | Description                                                                                                                                                                                                                                                                 |
| ----------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-c, --config <CONFIG>` | `mainnet` | Chain config: `mainnet`, `sepolia`, `devnet`, or a path to a config file                                                                                                                                                                                                    |
| `--conductor-rpc <URL>` |           | Bootstrap conductor JSON-RPC URL for runtime cluster discovery when the chain config has no hardcoded conductor list. Used by `basectl conductor` and `basectl sequencer`. If omitted, basectl uses `discovery.bootstrap_rpc` from config. Set via `BASECTL_CONDUCTOR_RPC`. |

The built-in mainnet and Sepolia configs target a local node at
`http://127.0.0.1:8545` (EL) and `http://127.0.0.1:9545` (CL). Their
`public_rpc` values retain the hosted endpoints for network-reference reads,
including tip comparisons and upgrade monitoring. Local-node commands do not
silently fall back when the local node is unavailable. Use the command-specific
RPC flags or a config override to target different endpoints.

## Commands

### `basectl monitor`

Opens the interactive TUI. With no subcommand, opens the Home view.
The top-right badge shows the active EL and CL endpoints. Press `e` from any
non-input view to switch the EL between the configured `rpc` and `public_rpc`;
the CL endpoint is unchanged. Switching rebuilds the active monitors so their
background requests reconnect to the selected EL endpoint.

| Command                  | Alias | Description                                      |
| ------------------------ | ----- | ------------------------------------------------ |
| `monitor`                |       | TUI Home view                                    |
| `monitor conductor`      | `co`  | HA conductor cluster monitor                     |
| `monitor da`             | `d`   | DA backlog monitor                               |
| `monitor flashblocks`    | `f`   | Flashblocks TUI monitor                          |
| `monitor command-center` | `cc`  | Combined command center view                     |
| `monitor upgrades`       | `u`   | Network upgrade activation countdown and history |
| `monitor config`         | `c`   | Chain configuration view                         |

### `basectl block <REF>`

Inspects a single L2 block via `eth_getBlockByHash` or `eth_getBlockByNumber`
(alloy dispatches based on the reference shape) and prints either an aligned
key-value table (default) or the full block as pretty JSON (`--json`).
Visible alias: `b`.

`<REF>` accepts:

- A decimal block number (e.g. `42417649`)
- A `0x`-hex block number (e.g. `0x2871c71`)
- A tag: `latest`, `safe`, `finalized`, `earliest`
- A 32-byte block hash (`0x` + 64 hex chars)

Hash lookups can return blocks regardless of canonical-chain status — orphans
and reorged-out heads are also fetchable by hash. The `pending` tag is not
supported (alloy's typed block can't deserialize null number/hash).

| Flag     | Description                                                                                                                                                                                                                                                                           |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--json` | Emit humanized JSON (decoded numeric values, ISO + local timestamps, `network`/`reference` context fields) instead of the key-value table.                                                                                                                                            |
| `--raw`  | With `--json`, emit the JSON-RPC wire format (camelCase field names, hex-string quantities, no `network`/`reference` wrapper) instead of the humanized form. Useful for round-tripping through `cast` or other JSON-RPC-aware tooling. Errors at parse time if used without `--json`. |

Pretty mode converts hex quantities to decimal and Unix timestamps to
`YYYY-MM-DD HH:MM:SS UTC`. Humanized JSON (`--json`) decodes numeric values
(`number: 42417649`, `gasUsed: 5345789`, `baseFeePerGasWei: 5000000`) and
gives you a nested `timestamp` object with `unix`/`utc`/`local` fields so
the operator's wall clock is readable without timezone math. Raw JSON
(`--json --raw`) preserves the alloy/JSON-RPC wire format with hex
quantities at the top level — byte-equivalent to `cast block --json`.

### `basectl sync-status`

Reports the rollup node's `optimism_syncStatus` (CL) joined with the EL's
`eth_syncing` state, plus a public-RPC tip reference for cross-checking.
One round-trip each, run in parallel; the CL/EL pair short-circuits on
failure, the tip reference is best-effort.

The CL response carries every L1/L2 head ref the rollup node knows about,
each with a block number, hash, and Unix timestamp. Pretty mode prints an
aligned key-value table; humanized JSON adds a precomputed `safeLagSeconds`
/ `safeLagBlocks` pair (`unsafe` minus `safe`) so consumers don't have to
re-derive lag from raw timestamps.

When the EL is mid-sync (`eth_syncing` returns the `Info(...)` variant),
the output also surfaces `processedBlocks` (`current - starting`) and
`remainingBlocks` (`highest - current`) so operators can quantify the gap
instead of just seeing "syncing: true."

A `tip_reference` row compares the local node's unsafe L2 head against the
preset's public RPC URL (`https://mainnet.base.org/`,
`https://sepolia.base.org/`, or `http://localhost:7545` for devnet). Status
is one of `caught_up` (within ±N blocks of the reference, where N is the
`--tip-tolerance` flag — default 5), `behind`, `ahead`, or `unavailable`
(public RPC unreachable).

| Flag                       | Description                                                                                                                                                                                                                                                                                           |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--el-rpc <URL>`           | Override the execution-layer RPC URL. Defaults to the chain config's local `rpc` field.                                                                                                                                                                                                              |
| `--cl-rpc <URL>`           | Override the consensus-node RPC URL. Defaults to the chain config's `consensus_node_rpc` field.                                                                                                                                                                                                       |
| `--tip-tolerance <BLOCKS>` | Block tolerance for the tip-reference `caught_up` classification. Within ±this many blocks of the public reference, the local node is reported as `caught_up`; otherwise `behind` or `ahead`. Default `5` ≈ ~10s at Base's 2s block time. Use `0` for strict alerting, larger values to dampen noise. |
| `--json`                   | Emit humanized JSON (decoded numeric values, ISO + local timestamps, precomputed `safeLag*`, `tipReference` object, `elSyncInfo` with `processedBlocks` / `remainingBlocks`) instead of the key-value table.                                                                                          |
| `--raw`                    | With `--json`, emit the alloy-typed `optimism_syncStatus` wire format instead of the humanized form. Errors at parse time if used without `--json`.                                                                                                                                                   |

### `basectl p2p`

P2P inspection and single-peer management commands for execution and
consensus layers.

- `basectl p2p info` shows the advertised endpoint per layer plus peer counts,
  and the CL max peer count when the consensus RPC reports it.
- `basectl p2p peers` shows the connected peer list per layer.
- `basectl p2p reachability <TARGET>` asks the Base telemetry service to open
  an independent connection to a node's advertised p2p endpoint.
  `enode://...` probes the execution layer (TCP, encrypted identity handshake,
  and devp2p Hello exchange); `enr:...` or `/ip4/.../tcp/.../p2p/<peer-id>`
  probes the consensus layer (TCP, Noise handshake against the advertised
  identity, and libp2p identify).
- `basectl p2p add-peer <TARGET>` connects one peer. `enode://...` routes to
  the execution layer; `enr:...` or `/.../p2p/<peer-id>` routes to the
  consensus layer.
- `basectl p2p remove-peer <TARGET>` disconnects one peer. `enode://...` routes
  to the execution layer; any other non-empty target is treated as a bare
  consensus libp2p peer ID. ENR records and multiaddrs are rejected for removal.
- `basectl p2p ban <TARGET>` bans one peer. `enode://...` routes to the execution
  layer; a bare libp2p peer ID routes to the consensus layer. ENR records and
  multiaddrs are rejected. CL bans also attempt to disconnect the peer
  immediately.
- `basectl p2p unban <TARGET>` unbans one execution or consensus peer using the
  same target routing, with the same ENR and multiaddr rejection. It does not
  reconnect the peer.
- `basectl p2p unban-all` unbans every peer currently banned by the consensus
  layer RPC.

Read-only p2p commands and single-peer actions support:

| Flag             | Description                                                                                                                                                                            |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Defaults to the chain config's local `rpc` field.             |
| `--cl-rpc <URL>` | Override the consensus-node RPC URL. Defaults to the chain config's `consensus_node_rpc` field.    |

Read-only p2p commands also support:

| Flag     | Description                                                                                                                  |
| -------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `--json` | Emit humanized JSON instead of the pretty table output.                                                                      |
| `--raw`  | With `--json`, emit raw nested RPC payloads instead of the humanized summary. Errors at parse time if used without `--json`. |

`p2p reachability` uses the selected config's L2 RPC to detect the live chain,
then routes the request to the hosted Base mainnet or Base Sepolia telemetry
service. The default config remains mainnet. Unsupported chains and failed
network detection return an error. The command supports `--json` and exits
non-zero when the probe completes with any outcome other than `reachable`, so
scripts can rely on the exit code.

The returned `stage` shows where the check stopped:

- `tcp_connect`: opening the node's advertised TCP address.
- `encrypted_handshake`: authenticating the encrypted connection using the enode identity.
- `devp2p_hello`: exchanging the Ethereum devp2p Hello message.

Destructive p2p commands also support:

| Flag     | Description                                                                                                                                                                              |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--yes`  | Skip the interactive confirmation prompt. By default, destructive p2p commands print the exact action and wait for `y` or `yes`; empty input and every other answer abort without error. |
| `--json` | Emit a structured action outcome instead of pretty text. Requires `--yes` so scripts do not hang on an interactive prompt.                                                               |

Important EL RPC note:

- EL peer count comes from `net_peerCount`, so it works on many restricted or public-style EL RPCs.
- EL advertised endpoint data (`admin_nodeInfo`) and EL peer listings (`admin_peers`) require an admin-enabled EL RPC.
- If the EL RPC does not expose those admin methods, `basectl p2p` degrades gracefully: EL peer count still appears, but EL endpoint fields or EL peer listings show as unavailable / `null`.
- EL ban/unban uses `admin_banPeer` and `admin_unbanPeer`, which require an EL
  implementation that exposes those admin methods. Reth trusted peers must be
  removed from the trusted set before they can be banned; because reth silently
  ignores a ban on a trusted peer, `basectl p2p ban` first checks `admin_peers`
  and fails fast when the target is a currently-connected trusted peer (a
  trusted peer with no live session is not detectable this way).
- CL data comes from `opp2p_self`, `opp2p_peerStats`, and `opp2p_peers(true)` on the consensus RPC.
- When exposed by the node, `opp2p_peerStats` also additively reports `maxPeerCount`, the configured CL max peer count.
- CL ban/unban commands use `opp2p_blockPeer`, `opp2p_unblockPeer`, and `opp2p_listBlockedPeers` underneath.
- `unban-all` remains CL-only because the EL admin API does not expose a banned-peer listing.

### `basectl txpool`

Transaction-pool inspection and clearing commands for one execution-layer node.
By default the command uses the selected config's `rpc` field. Pass
`--el-rpc <URL>` to target a specific admin-enabled node directly. The global
`--conductor-rpc` flag is ignored.

- `basectl txpool pending [SENDER]` shows pending txpool transactions.
- `basectl txpool queued [SENDER]` shows queued txpool transactions.
- `basectl txpool all [SENDER]` shows pending and queued txpool transactions.
- `basectl txpool clear` clears the whole txpool through upstream Reth
  `admin_clearTxpool`.
- `basectl txpool clear --sender <ADDRESS>` drops every txpool transaction for
  one sender through Base `admin_dropSenderTransactions`.

Read-only txpool commands support:

| Flag             | Description                                                                                                                                                                                                                                   |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Defaults to the chain config's `rpc` field.                                                                                                                                                             |
| `--json`         | Emit humanized JSON with `network`, `rpc`, `scope`, optional `sender`, counts, sender summaries, and decoded transaction rows.                                                                                                                |
| `--raw`          | With `--json`, emit the txpool wire shape (`TxpoolContent` for unfiltered reads, `TxpoolContentFrom` for sender-filtered reads), scoped to the selected `pending`, `queued`, or `all` command. Errors at parse time if used without `--json`. |

Destructive txpool clearing supports:

| Flag                 | Description                                                                                                                                                                                 |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--sender <ADDRESS>` | Drop only transactions from one sender instead of clearing the whole pool.                                                                                                                  |
| `--el-rpc <URL>`     | Override the execution-layer RPC URL. Destructive txpool calls usually require an admin-enabled node RPC.                                                                                   |
| `--yes`              | Skip the interactive confirmation prompt. By default, `clear` prints the exact target and waits for `y` or `yes`; empty input and every other answer abort without error.                   |
| `--json`             | Emit a structured action outcome instead of pretty text. Requires `--yes` so scripts do not hang on an interactive prompt. The `action` field is `clearTxpool` or `dropSenderTransactions`. |

`txpool pending`, `queued`, and `all` use Reth's `txpool_content` namespace, or
`txpool_contentFrom` when a sender filter is provided. `clear` does not support
dropping by individual transaction hash in v1.

Pretty read output includes the selected scope counts, per-sender nonce
summaries, and one transaction row per included tx with pool, sender, nonce,
hash, destination, value, gas, fee, and input byte length.

### `basectl conductor`

Conductor inspection and control commands for HA sequencer clusters.

- `basectl conductor status` shows cluster membership, leader, pause state,
  sequencer health, L1/L2 heads, and peer counts per node.
- `basectl conductor transfer-leader [TARGET]` transfers raft leadership away
  from the current leader, or to a named target node when `TARGET` is provided.
- `basectl conductor pause <NODE>` pauses op-conductor's control loop on one
  node.
- `basectl conductor unpause <NODE>` resumes op-conductor's control loop on one
  node.
- `basectl conductor pause-all` pauses op-conductor's control loop on every
  node in the cluster.
- `basectl conductor unpause-all` resumes op-conductor's control loop on every
  node in the cluster.

Conductor commands use the selected config's hardcoded `conductors` list when
present. Otherwise they discover the cluster via the `--conductor-rpc` bootstrap
URL or `discovery.bootstrap_rpc` in the config.

| Flag     | Description                                                                                |
| -------- | ------------------------------------------------------------------------------------------ |
| `--json` | For `status`, emit a structured cluster status summary instead of the pretty table output. |

Destructive conductor commands also support:

| Flag     | Description                                                                                                                   |
| -------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `--yes`  | Skip the interactive confirmation prompt.                                                                                     |
| `--json` | Emit a structured action outcome instead of pretty text. Requires `--yes` so scripts do not hang on interactive confirmation. |

Safety notes:

- `pause` / `unpause` prompts with the exact node name and conductor RPC URL.
- `transfer-leader` prompts with the target node or selected network.
- `pause-all` / `unpause-all` require typing the selected network name unless
  `--yes` is provided.
- Cluster-wide actions can partially succeed before one node fails. Pretty and
  JSON output include the success and failure sets, and the command exits
  non-zero when any node fails.

### `basectl sequencer`

Sequencer inspection and control commands for the nodes in an HA conductor
cluster.

- `basectl sequencer status [NODE]` shows sequencer activity, health, pause
  state, L1/L2 heads, and peer counts for every node, or for one selected node
  when `NODE` is provided.
- `basectl sequencer start <NODE> [UNSAFE_HEAD]` starts sequencing on one node
  through the consensus node's `admin_startSequencer` RPC.
- `basectl sequencer stop <NODE>` stops sequencing on one node through the
  consensus node's `admin_stopSequencer` RPC.

Like `basectl conductor`, sequencer commands use the selected config's
hardcoded `conductors` list when present and otherwise discover the live raft
membership from the global `--conductor-rpc` bootstrap URL or
`discovery.bootstrap_rpc` in the config.

When `start` omits `UNSAFE_HEAD`, basectl uses the node's currently observed
unsafe L2 hash. This matches the existing TUI behavior and the sequencer RPC's
safety contract: the requested hash must match the node's current engine unsafe
head.

| Flag     | Description                                                                             |
| -------- | --------------------------------------------------------------------------------------- |
| `--json` | For `status`, emit a structured JSON status summary instead of the pretty table output. |

Destructive sequencer commands also support:

| Flag     | Description                                                                                                                |
| -------- | -------------------------------------------------------------------------------------------------------------------------- |
| `--yes`  | Skip the interactive confirmation prompt.                                                                                  |
| `--json` | Emit a structured action outcome instead of pretty text. Requires `--yes` so scripts do not hang on an interactive prompt. |

Safety notes:

- `start` prompts with the exact node name, CL RPC URL, and unsafe head hash.
- `stop` prompts with the exact node name and CL RPC URL.
- After `start` / `stop`, basectl polls `admin_sequencerActive` for up to 12s
  before reporting success so an acknowledged RPC is not confused with the node
  actually reaching the desired state.

### `basectl doctor`

Runs read-only diagnostics for a single node and prints one row per check. The
command exits `1` if any check fails, and exits `0` when checks only pass, warn,
skip, or report informational context.

Doctor reads the selected config the same way as the other non-TUI commands:
built-in preset, optional YAML override, or explicit config path through global
`-c/--config`. By default it uses the config's `rpc`, `l1_rpc`, and
`consensus_node_rpc` values. Pass `--el-rpc` and `--cl-rpc` to point at a
different node.

Checks include declared network vs. live chain ID, p2p endpoint context,
canonical bootnode config context, advertised endpoint sanity,
telemetry-backed external EL and CL reachability, EL/CL peer counts, EL head
vs. public tip, safe-head recency, optional `reth.toml` headers/bodies limits,
consensus-node RPC presence, and L1 RPC reachability. Doctor does not mutate
node state. The effective `--el-rpc` chain ID selects the hosted Base mainnet or
Base Sepolia telemetry service; the reachability checks are skipped when
detection fails or the chain is unsupported. The external CL check also needs
`--cl-rpc` so the advertised ENR can be read from `opp2p_self`.

| Flag                                  | Description                                                                                                                                      |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--el-rpc <URL>`                      | Override the execution-layer RPC URL used for local-node checks. Defaults to the selected config's `rpc` field.                                  |
| `--cl-rpc <URL>`                      | Override the consensus-node RPC URL. If omitted and the selected config has no `consensus_node_rpc`, CL-dependent checks are skipped with hints. |
| `--reth-config <PATH>`                | Path to the local `reth.toml` file. If omitted, the reth limits check is skipped.                                                                |
| `--peer-warn-threshold <COUNT>`       | Connected peer count below which EL/CL peer checks warn. Default `5`.                                                                            |
| `--head-lag-warn-blocks <BLOCKS>`     | EL head lag behind the public tip above which doctor warns. Default `10`.                                                                        |
| `--head-lag-fail-blocks <BLOCKS>`     | EL head lag behind the public tip above which doctor fails. Default `20`.                                                                        |
| `--safe-recency-warn-blocks <BLOCKS>` | Safe-head lag behind unsafe head above which doctor warns. Default `150`.                                                                        |
| `--safe-recency-fail-blocks <BLOCKS>` | Safe-head lag behind unsafe head above which doctor fails. Default `300`.                                                                        |
| `--json`                              | Emit a humanized JSON report with `inputs`, `summary`, and `checks` instead of pretty text.                                                      |

### `basectl flashblocks`

Streams live flashblocks as newline-delimited JSON to stdout. For the
interactive view, use `basectl monitor flashblocks`.

### `basectl proofs`

Submits and inspects ZK proof requests on the internal prover service, used to
speed up finality for a block range when required.

- `basectl proofs finalize <GAME_OR_TX>` reads a dispute game's committed range
  from L1, requests and waits for its PLONK proof, then submits the proof on
  chain. The target may be the game proxy address or its direct factory
  creation transaction hash.
- `basectl proofs status <SESSION_ID>` shows status and result data for a
  submitted proof request.
- `basectl proofs list` lists submitted proof requests.
- `basectl proofs games [GAME_ADDRESS]` lists recent dispute games from the L1
  `DisputeGameFactory`, or inspects one game in detail when an address is
  given.
- `basectl proofs propose <GAME_ADDRESS> --prover-address <ADDRESS>` requests
  a PLONK proposal proof matched to an existing dispute game. The block range,
  L1 head, and intermediate root interval are read from the game so the proof
  verifies against the game's on-chain state.
- `basectl proofs submit <GAME_ADDRESS>` fetches the
  completed PLONK proof from the prover service and sends the
  `AggregateVerifier.verifyProposalProof` transaction to the game on L1.

The `finalize`, `propose`, `submit`, `status`, and `list` commands resolve the prover-service endpoint from the `--prover-rpc`
flag, then the `BASECTL_PROVER_RPC` environment variable, then the selected
config's `prover_rpc` field. The built-in presets ship without a `prover_rpc`
because the prover service is internal, so one of the three must be provided.

`proofs finalize` supports:

| Flag | Description |
|------|-------------|
| `--private-key-file <PATH>` | File containing the hex private key of the L1 wallet that is committed into the proof and submits the final transaction. When omitted, the key is read from `BASECTL_SUBMITTER_PRIVATE_KEY`. |
| `--zk-backend <BACKEND>` | ZK proving backend: `network` (default, Succinct Prover Network, paid in PROVE) or `cluster`. Use `proofs propose --zk-backend dry-run` for sizing; dry-run cannot finalize because it produces no proof bytes. |
| `--session-id <ID>` | Explicit proof session ID. If omitted, basectl derives one from the network, backend, game, block range, checkpoint stride, and submitter wallet. |
| `--intermediate-root-interval <N>` | Checkpoint stride override. Only needed when the game type has no registered implementation to read `INTERMEDIATE_BLOCK_INTERVAL` from; otherwise the flag must match that canonical value. |
| `--prover-rpc <URL>` | Prover-service RPC URL. Also `BASECTL_PROVER_RPC` or config `prover_rpc`. |
| `--factory <ADDRESS>` | `DisputeGameFactory` address. Also config `proofs.dispute_game_factory`. |
| `--l1-rpc <URL>` | L1 RPC URL. Also config `l1_rpc`. |
| `--yes` | Skip the interactive confirmation prompt. By default, `finalize` prints the exact target and waits for `y` or `yes`; every other answer aborts without error. |
| `--json` | Emit a structured JSON action outcome instead of pretty text. Requires `--yes` so scripts do not hang on an interactive prompt. |

`proofs status` supports:

| Flag | Description |
|------|-------------|
| `--prover-rpc <URL>` | Prover-service RPC URL. Also `BASECTL_PROVER_RPC` or config `prover_rpc`. |
| `--json` | Emit humanized JSON instead of pretty text. |
| `--raw` | With `--json`, emit the prover-service wire shape instead of the humanized summary. Errors at parse time if used without `--json`. |

`proofs list` supports:

| Flag | Description |
|------|-------------|
| `--status <STATUS>` | Only list proofs with this status: `queued`, `running`, `succeeded`, or `failed`. |
| `--offset <N>` | Number of rows to skip. Default `0`. |
| `--limit <N>` | Maximum rows to return. Default `50`. |
| `--prover-rpc <URL>` | Prover-service RPC URL. Also `BASECTL_PROVER_RPC` or config `prover_rpc`. |
| `--json` | Emit humanized JSON instead of pretty text. |

`proofs games` reads L1 directly (no prover service required). The factory
address comes from `--factory` or the config's `proofs.dispute_game_factory`;
the L1 endpoint comes from `--l1-rpc` or the config's `l1_rpc`. Filtered lists
scan at most the newest 256 factory games; pretty output warns and JSON sets
`searchTruncated` when older matches may exist. The `--limit`, `--game-type`,
and `--missing-zk` flags apply only when listing and conflict with
`GAME_ADDRESS`.

| Flag | Description |
|------|-------------|
| `--limit <N>` | Maximum games to list, scanning backwards from the newest. Default `20`, maximum `100`. |
| `--game-type <TYPE>` | Only list games of this game type. |
| `--missing-zk` | Only list games whose ZK proof slot is still empty. |
| `--factory <ADDRESS>` | `DisputeGameFactory` address. Also config `proofs.dispute_game_factory`. |
| `--l1-rpc <URL>` | L1 RPC URL. Also config `l1_rpc`. |
| `--json` | Emit humanized JSON instead of pretty text. |

`proofs propose` reads the target game from L1 and submits a game-matched
`snark_plonk` request to the prover service. The game must be in progress with
an empty ZK proof slot. The proof journal commits to `--prover-address` as
the proposer, so the later `verifyProposalProof` L1 transaction must be sent
from exactly that wallet — a proof generated for one address cannot be
submitted from another.

| Flag | Description |
|------|-------------|
| `--prover-address <ADDRESS>` | Required. L1 wallet that will submit the proof on chain. |
| `--zk-backend <BACKEND>` | ZK proving backend: `network` (default, Succinct Prover Network, paid in PROVE), `cluster`, or `dry-run`. |
| `--session-id <ID>` | Explicit proof session ID. If omitted, derived from the network name, ZK backend, game address, block range, checkpoint stride, and prover address. |
| `--intermediate-root-interval <N>` | Checkpoint stride override. Only needed when the game type has no registered implementation to read `INTERMEDIATE_BLOCK_INTERVAL` from; otherwise the flag must match that canonical value. |
| `--wait` | Poll the prover service until the proof succeeds or fails. |
| `--prover-rpc <URL>` | Prover-service RPC URL. Also `BASECTL_PROVER_RPC` or config `prover_rpc`. |
| `--factory <ADDRESS>` | `DisputeGameFactory` address. Also config `proofs.dispute_game_factory`. |
| `--l1-rpc <URL>` | L1 RPC URL. Also config `l1_rpc`. |
| `--yes` | Skip the interactive confirmation prompt. |
| `--json` | Emit a structured JSON action outcome instead of pretty text. Requires `--yes`. |

`proofs submit` completes the standalone proving workflow: it fetches the
completed PLONK proof for the game from the prover service and sends
`AggregateVerifier.verifyProposalProof(proof)` to the game on L1, waiting for
the transaction to be mined. The signing wallet must be exactly the
`--prover-address` the proof was proposed with; the contract rejects any other
sender with `InvalidSigner`. Before sending, basectl re-reads the game and
refuses to spend gas when the game is no longer in progress or already has a
ZK proof.

When `--session-id` is omitted, basectl derives the same deterministic session
ID that `proofs propose` derives — from the network name, ZK backend, game
address, block range, checkpoint stride, and the submitting wallet's address —
so a proof proposed and submitted with the same wallet needs no session
bookkeeping.

| Flag | Description |
|------|-------------|
| `--private-key-file <PATH>` | File containing the hex private key of the L1 wallet that signs and pays for the transaction. When omitted, the key is read from `BASECTL_SUBMITTER_PRIVATE_KEY`. Must control the `--prover-address` used at propose time. |
| `--session-id <ID>` | Explicit proof session ID. If omitted, derived as described above. |
| `--zk-backend <BACKEND>` | ZK backend the proof was proposed with (`network` default). Only used for session ID derivation. |
| `--intermediate-root-interval <N>` | Checkpoint stride the proof was proposed with. Only needed when the game's committed roots do not derive one; only used for session ID derivation. |
| `--wait` | Poll the prover service until the proof completes before submitting. |
| `--prover-rpc <URL>` | Prover-service RPC URL. Also `BASECTL_PROVER_RPC` or config `prover_rpc`. |
| `--factory <ADDRESS>` | `DisputeGameFactory` address. Also config `proofs.dispute_game_factory`. |
| `--l1-rpc <URL>` | L1 RPC URL. Also config `l1_rpc`. |
| `--yes` | Skip the interactive confirmation prompt. |
| `--json` | Emit a structured JSON action outcome instead of pretty text. Requires `--yes`. |

## Examples

### `basectl monitor`

```sh
# Open TUI on mainnet
basectl monitor

# Open TUI on devnet
basectl -c devnet monitor

# Open the conductor view directly
basectl monitor conductor
```

### `basectl flashblocks`

```sh
# Stream flashblocks as JSONL on sepolia
basectl -c sepolia flashblocks
```

### `basectl block`

```sh
# Inspect the latest block on sepolia
basectl -c sepolia block latest

# Decimal and 0x-hex refs produce identical output apart from the `reference` row
basectl -c sepolia block 42417649
basectl -c sepolia block 0x2871c71

# JSON mode pipes cleanly into jq (header fields are top-level, hex quantities preserved)
basectl -c mainnet block --json finalized | jq '{number, hash, gasUsed, baseFeePerGas}'

# Use the visible alias `b`
basectl -c mainnet b latest

# Look up a block by 32-byte hash (canonical, orphan, or reorged-out)
basectl -c sepolia block 0x9fa0d82dfdf395d552e92caec6a9d5482c53f1800e8f3ff29994b7a431447148

# Humanized JSON: decoded numbers, nested timestamp with utc + local, network context
basectl -c sepolia block --json latest | jq '{number, gasUsed, baseFeePerGasWei, timestamp}'

# Raw (wire) JSON: same shape as `cast block --json`, useful for round-tripping
basectl -c mainnet block --json --raw finalized | jq '{number, gasUsed, baseFeePerGas}'
```

### `basectl sync-status`

```sh
# Sync status against a devnet (consensus_node_rpc is set in the devnet preset)
basectl -c devnet sync-status

# Sync status against a local Sepolia node
basectl -c sepolia sync-status

# Humanized JSON shows precomputed safe-head lag for downstream tooling
basectl -c sepolia sync-status --json | jq '{safeLagSeconds, safeLagBlocks, elActivelySyncing}'
```

### `basectl p2p`

```sh
# P2P endpoint summary for a node
basectl -c sepolia p2p info

# P2P peers as JSON
basectl -c sepolia p2p peers --json | jq '{el: .el | length, cl: .cl | length}'

# Probe an explicit EL enode from the telemetry service's network
basectl -c sepolia p2p reachability enode://<node-id>@203.0.113.10:30303 --json

# Probe a consensus peer by public-IPv4 libp2p multiaddr
basectl -c sepolia p2p reachability /ip4/203.0.113.10/tcp/9222/p2p/16Uiu2HAm... --json

# Add an execution-layer peer after confirmation
basectl -c sepolia p2p add-peer enode://<node-id>@203.0.113.10:30303 --el-rpc https://your-el.example/

# Connect a consensus peer non-interactively and emit JSON
basectl -c sepolia p2p add-peer enr:<record> --cl-rpc https://your-cl.example/ --yes --json | jq .

# Connect a consensus peer by raw libp2p multiaddr
basectl -c sepolia p2p add-peer /ip4/203.0.113.10/tcp/9000/p2p/16Uiu2HAm... --cl-rpc https://your-cl.example/ --yes

# Remove a consensus peer by bare libp2p peer ID
basectl -c sepolia p2p remove-peer 16Uiu2HAm... --cl-rpc https://your-cl.example/

# Ban an execution peer through an admin-enabled EL RPC
basectl -c sepolia p2p ban enode://<node-id>@203.0.113.10:30303 --el-rpc https://your-el.example/

# Unban an execution peer non-interactively and emit JSON
basectl -c sepolia p2p unban enode://<node-id>@203.0.113.10:30303 --el-rpc https://your-el.example/ --yes --json | jq .

# Ban a consensus peer and best-effort disconnect it immediately
basectl -c sepolia p2p ban 16Uiu2HAm... --cl-rpc https://your-cl.example/

# Unban a consensus peer non-interactively and emit JSON
basectl -c sepolia p2p unban 16Uiu2HAm... --cl-rpc https://your-cl.example/ --yes --json | jq .

# Unban all currently banned consensus peers
basectl -c sepolia p2p unban-all --cl-rpc https://your-cl.example/ --yes

# If the EL RPC is restricted, EL peer count still works but EL admin-backed fields may be unavailable
basectl -c sepolia p2p info --el-rpc https://your-public-el.example/ --cl-rpc https://your-cl.example/
```

### `basectl conductor`

```sh
# Show devnet conductor cluster status
basectl -c devnet conductor status

# Conductor status as structured JSON
basectl -c devnet conductor status --json | jq '{leader, paused, nodes: [.nodes[].name]}'

# Transfer conductor leadership to a target node after confirmation
basectl -c devnet conductor transfer-leader op-conductor-1

# Pause and unpause one conductor node
basectl -c devnet conductor pause op-conductor-0
basectl -c devnet conductor unpause op-conductor-0 --yes --json | jq .

# Cluster-wide conductor actions require typed confirmation, or --yes for scripts
basectl -c devnet conductor pause-all
basectl -c devnet conductor unpause-all --yes --json | jq .
```

### `basectl sequencer`

```sh
# Show sequencer state for every devnet conductor node
basectl -c devnet sequencer status

# Show sequencer state for one node as JSON
basectl -c devnet sequencer status op-conductor-0 --json | jq .

# Stop a sequencer node and capture the returned unsafe head
basectl -c devnet sequencer stop op-conductor-0 --yes --json | jq '{node, unsafeHead}'

# Start a sequencer node using its currently observed unsafe head
basectl -c devnet sequencer start op-conductor-0 --yes

# Start a sequencer node with an explicit unsafe head hash
basectl -c devnet sequencer start op-conductor-0 0x1111111111111111111111111111111111111111111111111111111111111111 --yes --json | jq .
```

### `basectl doctor`

```sh
# Run doctor with values from the selected config
basectl -c mainnet doctor

# Run doctor against a specific node
basectl -c mainnet doctor --el-rpc https://your-el.example/ --cl-rpc https://your-cl.example/

# Include an external EL reachability probe
basectl -c mainnet doctor --el-rpc https://your-el.example/

# Include local reth headers/bodies limit validation and JSON output
basectl -c mainnet doctor --el-rpc https://your-el.example/ --cl-rpc https://your-cl.example/ --reth-config /etc/reth/reth.toml --json

# Prove and finalize a dispute game after confirmation
basectl -c zeronet proofs finalize 0xGAME_ADDRESS --zk-backend cluster

# The direct factory creation transaction identifies the same game
basectl -c zeronet proofs finalize 0xCREATION_TX_HASH --zk-backend cluster

# Check the status of a submitted proof request
basectl -c devnet proofs status <SESSION_ID> --prover-rpc https://your-prover.example/

# List running proof requests as JSON
basectl -c devnet proofs list --status running --prover-rpc https://your-prover.example/ --json | jq .
```
