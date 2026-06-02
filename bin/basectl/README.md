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

### `basectl flashblocks`

Streams live flashblocks as newline-delimited JSON to stdout. This is the only
non-TUI subcommand. For the interactive view, use `basectl monitor flashblocks`.

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
```
