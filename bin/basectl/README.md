# `basectl`

The Base infrastructure control CLI.

## Usage

```
basectl [OPTIONS] [COMMAND]
```

Global options:

| Flag | Default | Description |
|------|---------|-------------|
| `-c, --config <CONFIG>` | `mainnet` | Chain config: `mainnet`, `sepolia`, `devnet`, or a path to a config file |
| `--conductor-rpc <URL>` | | Bootstrap conductor JSON-RPC URL for runtime cluster discovery when the chain config has no hardcoded conductor list. Used by `basectl conductor` and `basectl sequencer`. If omitted, basectl uses `discovery.bootstrap_rpc` from config. Set via `BASECTL_CONDUCTOR_RPC`. |

## Commands

### `basectl monitor`

Opens the interactive TUI. With no subcommand, opens the Home view.

| Command | Alias | Description |
|---------|-------|-------------|
| `monitor` | | TUI Home view |
| `monitor conductor` | `co` | HA conductor cluster monitor |
| `monitor da` | `d` | DA backlog monitor |
| `monitor flashblocks` | `f` | Flashblocks TUI monitor |
| `monitor command-center` | `cc` | Combined command center view |
| `monitor upgrades` | `u` | Network upgrade activation countdown and history |
| `monitor config` | `c` | Chain configuration view |

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

| Flag | Description |
|------|-------------|
| `--json` | Emit humanized JSON (decoded numeric values, ISO + local timestamps, `network`/`reference` context fields) instead of the key-value table. |
| `--raw` | With `--json`, emit the JSON-RPC wire format (camelCase field names, hex-string quantities, no `network`/`reference` wrapper) instead of the humanized form. Useful for round-tripping through `cast` or other JSON-RPC-aware tooling. Errors at parse time if used without `--json`. |

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

| Flag | Description |
|------|-------------|
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Defaults to the chain config's `rpc` field. |
| `--cl-rpc <URL>` | Override the consensus-node RPC URL. The mainnet and sepolia presets ship `consensus_node_rpc` unset, so non-devnet users must pass this flag (or set the field in their YAML config). |
| `--tip-tolerance <BLOCKS>` | Block tolerance for the tip-reference `caught_up` classification. Within ±this many blocks of the public reference, the local node is reported as `caught_up`; otherwise `behind` or `ahead`. Default `5` ≈ ~10s at Base's 2s block time. Use `0` for strict alerting, larger values to dampen noise. |
| `--json` | Emit humanized JSON (decoded numeric values, ISO + local timestamps, precomputed `safeLag*`, `tipReference` object, `elSyncInfo` with `processedBlocks` / `remainingBlocks`) instead of the key-value table. |
| `--raw` | With `--json`, emit the alloy-typed `optimism_syncStatus` wire format instead of the humanized form. Errors at parse time if used without `--json`. |

### `basectl p2p`

P2P inspection and single-peer management commands for execution and
consensus layers.

- `basectl p2p info` shows the advertised endpoint per layer plus peer counts,
  and the CL max peer count when the consensus RPC reports it.
- `basectl p2p peers` shows the connected peer list per layer.
- `basectl p2p add-peer <TARGET>` connects one peer. `enode://...` routes to
  the execution layer; `enr:...` or `/.../p2p/<peer-id>` routes to the
  consensus layer.
- `basectl p2p remove-peer <TARGET>` disconnects one peer. `enode://...` routes
  to the execution layer; any other non-empty target is treated as a bare
  consensus libp2p peer ID. ENR records and multiaddrs are rejected for removal.
- `basectl p2p ban <PEER_ID>` bans one consensus-layer peer and then attempts
  to disconnect it so the ban takes effect immediately.
- `basectl p2p unban <PEER_ID>` unbans one consensus-layer peer. It does not
  reconnect the peer.
- `basectl p2p unban-all` unbans every peer currently banned by the consensus
  layer RPC.

Read-only p2p commands and `add-peer` / `remove-peer` support:

| Flag | Description |
|------|-------------|
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Defaults to the chain config's `rpc` field. |
| `--cl-rpc <URL>` | Override the consensus-node RPC URL. The mainnet and sepolia presets ship `consensus_node_rpc` unset, so non-devnet users must pass this flag (or set the field in their YAML config). |

CL-only ban/unban commands support:

| Flag | Description |
|------|-------------|
| `--cl-rpc <URL>` | Override the consensus-node RPC URL. The mainnet and sepolia presets ship `consensus_node_rpc` unset, so non-devnet users must pass this flag (or set the field in their YAML config). |

Read-only p2p commands also support:

| Flag | Description |
|------|-------------|
| `--json` | Emit humanized JSON instead of the pretty table output. |
| `--raw` | With `--json`, emit raw nested RPC payloads instead of the humanized summary. Errors at parse time if used without `--json`. |

Destructive p2p commands also support:

| Flag | Description |
|------|-------------|
| `--yes` | Skip the interactive confirmation prompt. By default, destructive p2p commands print the exact action and wait for `y` or `yes`; empty input and every other answer abort without error. |
| `--json` | Emit a structured action outcome instead of pretty text. Requires `--yes` so scripts do not hang on an interactive prompt. |

Important EL RPC note:

- EL peer count comes from `net_peerCount`, so it works on many restricted or public-style EL RPCs.
- EL advertised endpoint data (`admin_nodeInfo`) and EL peer listings (`admin_peers`) require an admin-enabled EL RPC.
- If the EL RPC does not expose those admin methods, `basectl p2p` degrades gracefully: EL peer count still appears, but EL endpoint fields or EL peer listings show as unavailable / `null`.
- CL data comes from `opp2p_self`, `opp2p_peerStats`, and `opp2p_peers(true)` on the consensus RPC.
- When exposed by the node, `opp2p_peerStats` also additively reports `maxPeerCount`, the configured CL max peer count.
- CL ban/unban commands use `opp2p_blockPeer`, `opp2p_unblockPeer`, and `opp2p_listBlockedPeers` underneath, but the basectl command surface uses ban/unban terminology so it can stay consistent when EL ban support is added later.

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

| Flag | Description |
|------|-------------|
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Defaults to the chain config's `rpc` field. |
| `--json` | Emit humanized JSON with `network`, `rpc`, `scope`, optional `sender`, counts, sender summaries, and decoded transaction rows. |
| `--raw` | With `--json`, emit the txpool wire shape (`TxpoolContent` for unfiltered reads, `TxpoolContentFrom` for sender-filtered reads), scoped to the selected `pending`, `queued`, or `all` command. Errors at parse time if used without `--json`. |

Destructive txpool clearing supports:

| Flag | Description |
|------|-------------|
| `--sender <ADDRESS>` | Drop only transactions from one sender instead of clearing the whole pool. |
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Destructive txpool calls usually require an admin-enabled node RPC. |
| `--yes` | Skip the interactive confirmation prompt. By default, `clear` prints the exact target and waits for `y` or `yes`; empty input and every other answer abort without error. |
| `--json` | Emit a structured action outcome instead of pretty text. Requires `--yes` so scripts do not hang on an interactive prompt. The `action` field is `clearTxpool` or `dropSenderTransactions`. |

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

| Flag | Description |
|------|-------------|
| `--json` | For `status`, emit a structured cluster status summary instead of the pretty table output. |

Destructive conductor commands also support:

| Flag | Description |
|------|-------------|
| `--yes` | Skip the interactive confirmation prompt. |
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

| Flag | Description |
|------|-------------|
| `--json` | For `status`, emit a structured JSON status summary instead of the pretty table output. |

Destructive sequencer commands also support:

| Flag | Description |
|------|-------------|
| `--yes` | Skip the interactive confirmation prompt. |
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
specific node when the config points at shared/public endpoints.

Checks include declared network vs. live chain ID, p2p endpoint context,
canonical bootnode config context, advertised endpoint sanity, EL/CL peer counts,
EL head vs. public tip, safe-head recency, optional `reth.toml` headers/bodies
limits, consensus-node RPC presence, and L1 RPC reachability. Doctor does not
mutate node state and does not prove advertised ports are reachable from the
public internet; it reports what can be observed from local config and exposed
RPC metadata.

| Flag | Description |
|------|-------------|
| `--el-rpc <URL>` | Override the execution-layer RPC URL used for local-node checks. Defaults to the selected config's `rpc` field. |
| `--cl-rpc <URL>` | Override the consensus-node RPC URL. If omitted and the selected config has no `consensus_node_rpc`, CL-dependent checks are skipped with hints. |
| `--reth-config <PATH>` | Path to the local `reth.toml` file. If omitted, the reth limits check is skipped. |
| `--peer-warn-threshold <COUNT>` | Connected peer count below which EL/CL peer checks warn. Default `5`. |
| `--head-lag-warn-blocks <BLOCKS>` | EL head lag behind the public tip above which doctor warns. Default `10`. |
| `--head-lag-fail-blocks <BLOCKS>` | EL head lag behind the public tip above which doctor fails. Default `20`. |
| `--safe-recency-warn-blocks <BLOCKS>` | Safe-head lag behind unsafe head above which doctor warns. Default `150`. |
| `--safe-recency-fail-blocks <BLOCKS>` | Safe-head lag behind unsafe head above which doctor fails. Default `300`. |
| `--json` | Emit a humanized JSON report with `inputs`, `summary`, and `checks` instead of pretty text. |

### `basectl flashblocks`

Streams live flashblocks as newline-delimited JSON to stdout. For the
interactive view, use `basectl monitor flashblocks`.

## Examples

```sh
# Open TUI on mainnet
basectl monitor

# Open TUI on devnet
basectl -c devnet monitor

# Open the conductor view directly
basectl monitor conductor

# Stream flashblocks as JSONL on sepolia
basectl -c sepolia flashblocks

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

# Sync status against a devnet (consensus_node_rpc is set in the devnet preset)
basectl -c devnet sync-status

# Sync status against a public chain — requires explicit --cl-rpc since mainnet/sepolia presets ship without one
basectl -c sepolia sync-status --cl-rpc https://your-rollup-node.example/

# Humanized JSON shows precomputed safe-head lag for downstream tooling
basectl -c sepolia sync-status --cl-rpc https://your-rollup-node.example/ --json | jq '{safeLagSeconds, safeLagBlocks, elActivelySyncing}'

# P2P endpoint summary for a node
basectl -c sepolia p2p info --el-rpc https://your-el.example/ --cl-rpc https://your-cl.example/

# P2P peers as JSON
basectl -c sepolia p2p peers --el-rpc https://your-el.example/ --cl-rpc https://your-cl.example/ --json | jq '{el: .el | length, cl: .cl | length}'

# Add an execution-layer peer after confirmation
basectl -c sepolia p2p add-peer enode://<node-id>@203.0.113.10:30303 --el-rpc https://your-el.example/

# Connect a consensus peer non-interactively and emit JSON
basectl -c sepolia p2p add-peer enr:<record> --cl-rpc https://your-cl.example/ --yes --json | jq .

# Connect a consensus peer by raw libp2p multiaddr
basectl -c sepolia p2p add-peer /ip4/203.0.113.10/tcp/9000/p2p/16Uiu2HAm... --cl-rpc https://your-cl.example/ --yes

# Remove a consensus peer by bare libp2p peer ID
basectl -c sepolia p2p remove-peer 16Uiu2HAm... --cl-rpc https://your-cl.example/

# Ban a consensus peer and best-effort disconnect it immediately
basectl -c sepolia p2p ban 16Uiu2HAm... --cl-rpc https://your-cl.example/

# Unban a consensus peer non-interactively and emit JSON
basectl -c sepolia p2p unban 16Uiu2HAm... --cl-rpc https://your-cl.example/ --yes --json | jq .

# Unban all currently banned consensus peers
basectl -c sepolia p2p unban-all --cl-rpc https://your-cl.example/ --yes

# If the EL RPC is restricted, EL peer count still works but EL admin-backed fields may be unavailable
basectl -c sepolia p2p info --el-rpc https://your-public-el.example/ --cl-rpc https://your-cl.example/

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

# Run doctor with values from the selected config
basectl -c mainnet doctor

# Run doctor against a specific node
basectl -c mainnet doctor --el-rpc https://your-el.example/ --cl-rpc https://your-cl.example/

# Include local reth headers/bodies limit validation and JSON output
basectl -c mainnet doctor --el-rpc https://your-el.example/ --cl-rpc https://your-cl.example/ --reth-config /etc/reth/reth.toml --json
```
