# base-state-populate

Offline testing utility for seeding a reth MDBX database with a large number of ERC-20
balance slots (and, optionally, synthetic EOA accounts) plus the corresponding trie nodes.
It lets you construct a realistic multi-hundred-million-slot state offline so that builder
and load-test benchmarks run against a representative state size without syncing real chain
data.

The tool writes directly to the flat state tables and then rebuilds the affected storage and
account tries, so the resulting datadir is a valid target for `reth`/`base-reth` with a
correct state root.

## Subcommands

| Command | Description |
|---------|-------------|
| `populate` | Write balance slots, hashed storage, accounts, and trie nodes for a contract |
| `verify`   | Read back a sample of slots, count rows, and check trie nodes |

## Usage

Populate 700 million balance slots into the `_balances` mapping (slot 0) of a contract:

```bash
state-populate populate \
  --datadir      /path/to/reth/datadir \
  --contract     0xABCDEF0000000000000000000000000000000000 \
  --balance-slot 0x0000000000000000000000000000000000000000000000000000000000000000 \
  --count        700000000 \
  --balance      1000000000000000000
```

Also write synthetic EOA accounts and pre-seed a load test's sender addresses:

```bash
state-populate populate \
  --datadir           /path/to/reth/datadir \
  --contract          0xABCDEF0000000000000000000000000000000000 \
  --count             700000000 \
  --populate-accounts \
  --seed              12345 \
  --sender-count      1000
```

Verify the written data:

```bash
state-populate verify \
  --datadir  /path/to/reth/datadir \
  --contract 0xABCDEF0000000000000000000000000000000000 \
  --count    700000000
```

Repair stale tries on an existing dataset without rewriting slots:

```bash
state-populate populate --datadir /path/to/reth/datadir --contract 0xABCD… --trie-only
```

## How it works

- Balance slots for holder `i` are written at `keccak256(pad12(address_for_index(i)) ++ balance_slot)`,
  matching Solidity's `mapping(address => uint256)` layout. `--seed`/`--sender-count` additionally
  pre-seed the exact addresses a load generator derives, so its signers hold a balance.
- Writes go to `PlainStorageState`, `HashedStorages`, and (with `--populate-accounts`)
  `PlainAccountState` + `HashedAccounts`.
- Slots are generated in parallel, globally sorted, and appended in commit-sized chunks so MDBX
  extends leaf pages linearly instead of splitting B-tree nodes — critical for write throughput
  on copy-on-write filesystems.
- Storage trie nodes are computed via `StorageRoot::from_tx_hashed` and written to `StoragesTrie`;
  the account trie is updated via `StateRoot` and written to `AccountsTrie`, yielding a correct
  state root.

## Resource requirements

Slot generation buffers `count × 32` bytes per table in memory before sorting. For 700M slots
this is roughly 22 GB per table; run it on a machine with ~50+ GB of available RAM.
