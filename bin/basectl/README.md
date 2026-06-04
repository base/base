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

Inspects a single L2 block via `eth_getBlockByNumber` and prints either an
aligned key-value table (default) or the full block as pretty JSON (`--json`).
Visible alias: `b`.

`<REF>` accepts:

- A decimal block number (e.g. `42417649`)
- A `0x`-hex block number (e.g. `0x2871c71`)
- A tag: `latest`, `safe`, `finalized`, `earliest`

Block-hash references (a `0x`-prefixed 32-byte hex string) are explicitly
rejected — pass a number or tag instead.

| Flag | Description |
|------|-------------|
| `--json` | Emit pretty JSON instead of the key-value table. |

Pretty mode converts hex quantities to decimal and Unix timestamps to
`YYYY-MM-DD HH:MM:SS UTC`. JSON mode preserves the JSON-RPC wire format —
camelCase field names, hex-string quantities — so it round-trips cleanly
through any JSON-RPC-aware tool. All header fields are at the top level of
the JSON object (no `.header` wrapper), matching the `eth_getBlockByNumber`
response shape.

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
```
