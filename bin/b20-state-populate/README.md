# b20-state-populate

Offline tool for seeding a Reth MDBX database with 700 M B20 token balance slots and
the corresponding trie nodes, enabling reproducible B20 benchmark runs against a realistic
state size.

## Subcommands

| Command | Description |
|---------|-------------|
| `populate` | Write balance slots, hashed storage, and trie nodes for a single B20 token |
| `verify` | Read-back a sample of slots and report row counts |

## Usage

```bash
# Seed 700 million accounts into the token deployed by creator 0xABCD… with salt 0x00
b20-state-populate populate \
  --datadir /home/meyer9/snapshots/base-mainnet-b20-bench \
  --creator 0xABCDEF0000000000000000000000000000000000 \
  --salt    0x0000000000000000000000000000000000000000000000000000000000000000 \
  --count   700000000 \
  --balance 1000000000000000000

# Verify the written data
b20-state-populate verify \
  --datadir /home/meyer9/snapshots/base-mainnet-b20-bench \
  --creator 0xABCDEF0000000000000000000000000000000000 \
  --salt    0x0000000000000000000000000000000000000000000000000000000000000000 \
  --count   700000000
```

## Design

- All writes go to `PlainStorageState`, `HashedStorages`, `PlainAccountState`, and
  `HashedAccounts`.
- Storage trie nodes are computed via `StorageRoot::from_tx_hashed` and written to
  `StoragesTrie` (one pass over `HashedStorages` for the token address).
- The account trie is updated via `StateRoot::overlay_root_with_updates` (modifies only
  the single path for the token address in `AccountsTrie`).
- Writes are batched in chunks of 1 M to keep transactions manageable.
