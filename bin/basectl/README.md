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
```
