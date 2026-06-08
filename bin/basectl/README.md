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
| `--conductor-rpc <URL>` | `http://localhost:5545` | Bootstrap conductor JSON-RPC URL for runtime cluster discovery. Overrides any hardcoded conductor list in the chain config. Set via `BASECTL_CONDUCTOR_RPC`. |

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

Read-only P2P inspection commands for execution and consensus layers.

- `basectl p2p info` shows the advertised endpoint per layer plus peer counts.
- `basectl p2p peers` shows the connected peer list per layer.

Both commands support:

| Flag | Description |
|------|-------------|
| `--el-rpc <URL>` | Override the execution-layer RPC URL. Defaults to the chain config's `rpc` field. |
| `--cl-rpc <URL>` | Override the consensus-node RPC URL. The mainnet and sepolia presets ship `consensus_node_rpc` unset, so non-devnet users must pass this flag (or set the field in their YAML config). |
| `--json` | Emit humanized JSON instead of the pretty table output. |
| `--raw` | With `--json`, emit raw nested RPC payloads instead of the humanized summary. Errors at parse time if used without `--json`. |

Important EL RPC note:

- EL peer count comes from `net_peerCount`, so it works on many restricted or public-style EL RPCs.
- EL advertised endpoint data (`admin_nodeInfo`) and EL peer listings (`admin_peers`) require an admin-enabled EL RPC.
- If the EL RPC does not expose those admin methods, `basectl p2p` degrades gracefully: EL peer count still appears, but EL endpoint fields or EL peer listings show as unavailable / `null`.
- CL data comes from `opp2p_self`, `opp2p_peerStats`, and `opp2p_peers(true)` on the consensus RPC.

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

# If the EL RPC is restricted, EL peer count still works but EL admin-backed fields may be unavailable
basectl -c sepolia p2p info --el-rpc https://your-public-el.example/ --cl-rpc https://your-cl.example/
```
