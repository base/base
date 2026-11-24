# Differential trie protocol

This document defines the `trie` mode of the differential fuzz harness: how `base-diff-fuzz
trie-fuzz` asks an adapter to apply a series of blocks of state writes, given in hashed-key space,
to its Merkle Patricia trie and state root code over its own on-disk database, and what the adapter
reports after every block (state root, storage roots, and account and storage proofs). It compares
state root implementations directly, in process, without executing transactions or running nodes.

The transport is the one in [PROTOCOL.md](./PROTOCOL.md): one JSON request per line on stdin, one
JSON response line per request on stdout, logs on stderr, exit on end of input. A request the
adapter cannot process gets `"ok": false` with `error`.

## Request

```json
{
  "id": "trie-17-3",
  "mode": "trie",
  "trie": {
    "dir": "/tmp/diff-trie/job-3",
    "blocks": [
      {
        "number": "0x1",
        "ops": [
          { "op": "account", "key": "0x<32 bytes>", "nonce": "0x1", "balance": "0x10", "codeHash": "0x<32 bytes>" },
          { "op": "storage", "key": "0x<32 bytes>", "slot": "0x<32 bytes>", "value": "0x<32 bytes>" },
          { "op": "wipe", "key": "0x<32 bytes>" },
          { "op": "delete", "key": "0x<32 bytes>" }
        ],
        "proofs": [{ "key": "0x<32 bytes>", "slots": ["0x<32 bytes>"] }]
      },
      {
        "number": "0x2",
        "unwind": 1,
        "revert": [{ "op": "delete", "key": "0x<32 bytes>" }],
        "ops": [],
        "proofs": []
      }
    ]
  }
}
```

| Field | Meaning |
| --- | --- |
| `trie.dir` | A directory the adapter may use for this request's databases. It does not exist yet or is empty. The adapter creates its databases under it, and deletes them (and the directory) before it responds. Every request starts from an empty state: requests are independent. |
| `trie.blocks` | Blocks applied in order, starting from the empty trie (state root `0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421`). |
| `number` | The block number, a quantity. The first block is `0x1`. It is the parent's number plus one, where the parent is the previous block or, after an unwind, the block the chain unwound to. Adapters that key history by block number use it. |
| `ops` | The block's writes, applied in order. Keys and slots are 32-byte hashed keys (the keccak of an address or a storage slot as the trie stores it). Implementations must not hash them again. |
| `unwind` | Optional, default 0. Before this block, the chain drops its last `unwind` blocks, so the block builds on the state after block `number - 1`. `unwind` is at most the number of blocks applied so far. |
| `revert` | Only when `unwind > 0` (omitted when empty): writes that turn the state at the tip back into the state after block `number - 1`. An adapter that cannot unwind natively applies `revert` followed by `ops` as one block. An adapter that unwinds natively may ignore `revert`. |
| `proofs` | Reads after the block's writes are committed: for each `key`, an account proof, and for each of `slots`, a storage proof. Keys and slots may be touched by the block or not, and may or may not exist. A key may repeat. |

Requests stay within these limits, which match needlefish's per-block changeset bounds: at most
256 blocks; per block at most 2048 `revert` plus `ops` writes, 8192 storage changes (a `storage`
write counts one, and a `wipe` or `delete` counts every slot the account holds when it applies,
in `revert` and `ops` together), and 256 proof targets of at most 64 slots each.

### Preimages

Some requests use only keys whose preimages the harness knows, for implementations keyed by
address and slot index rather than by hashed key. In those, every op and proof target carries them:
`address` (20 bytes, `key = keccak(address)`) on every op and on proof targets, `slotKey` (32
bytes, `slot = keccak(slotKey)`) on `storage` ops, and `slotKeys` (parallel to `slots`, so empty
when `slots` is) on proof targets. Other requests carry no preimages: an implementation that
needs them omits its results (its variant is absent from `variants`). An adapter refuses
(`"ok": false`) a request whose preimage does not hash to its key.

### Ops

Within a block, ops apply in order, and a key or slot may be written several times: the later write
wins, and a write may set a value back to what the block started with. The state after a block is
the result of applying every op.

| `op` | Effect |
| --- | --- |
| `account` | Sets the account at `key` to `nonce`, `balance` (quantities up to 256 bits; nonce up to 64 bits), and `codeHash`. If the account does not exist, it is created with empty storage. If it exists, its storage is kept. The harness never sends an empty account (nonce 0, balance 0, and the empty code hash): since EIP-7523 none can exist, and clients treat one as absent. An `account` op may carry `code`, hex bytecode of at most 24,576 bytes (EIP-170) whose keccak is `codeHash`, for implementations that store code; others ignore it. A request has at most 1024 distinct codes and 1 MiB of code. |
| `storage` | Sets storage `slot` of the account at `key` to the 32-byte `value`. A zero value clears the slot (removes it from the storage trie; clearing a slot that is not set changes nothing). The harness only sends `storage` for an account that exists when the op applies. |
| `wipe` | Clears all storage of the account at `key`, which keeps existing with its fields. Later `storage` ops in the block start from empty storage. A wipe of an account that does not exist changes nothing. |
| `delete` | Removes the account at `key` and all its storage. Deleting an account that does not exist changes nothing. An `account` op after a `delete` in the same or a later block recreates the account with empty storage. |

The storage root of an account is the root of its storage trie, which the account leaf holds as its
fourth field: the leaf value is `rlp([nonce, balance, storageRoot, codeHash])`, and a storage leaf
value is the RLP of the slot value as a big-endian integer with leading zeros stripped.

## Response

```json
{
  "id": "trie-17-3",
  "ok": true,
  "blocks": [
    {
      "number": "0x1",
      "stateRoot": "0x...",
      "storageRoots": { "0x<key>": "0x<root>" },
      "proofs": [
        {
          "key": "0x...",
          "account": { "nonce": "0x1", "balance": "0x10", "codeHash": "0x...", "storageRoot": "0x..." },
          "accountProof": ["0x<rlp node>", "..."],
          "storage": [{ "slot": "0x...", "value": "0x<32 bytes>", "proof": ["0x<rlp node>"] }]
        }
      ]
    }
  ],
  "variants": {
    "patricia-rocksdb": [{ "number": "0x1", "stateRoot": "0x..." }],
    "flat": [{ "number": "0x1", "stateRoot": "0x...", "storageRoots": { "0x...": "0x..." } }]
  },
  "coverage": { "nf:edge:1234": 1 }
}
```

| Field | Meaning |
| --- | --- |
| `blocks` | One result per request block, in order, from the adapter's primary implementation: the code path its node uses to commit a block's state. |
| `stateRoot` | The state root after the block. |
| `storageRoots` | For every key that an op of this block (`ops`, and `revert` when present) names and that exists after the block: its storage root. Lower-case keys. |
| `proofs` | One entry per request `proofs` entry, in order. `account` is the account after the block, or `null` if it does not exist. `accountProof` lists the trie nodes from the state root along the key's path: every node visited, including the last one (the leaf, or the node that shows the key is absent). For the empty trie it is empty. `storage` has one entry per requested slot, in order: `value` (zero when unset or when the account does not exist) and `proof`, the storage trie nodes from the account's storage root along the slot's path (empty for an empty storage trie or an account that does not exist). |
| `code` | Optional, in a proof whose account exists: the account's code, from implementations that store code. `"0x"` for the empty code hash; absent when the implementation does not hold the code. The harness checks that it hashes to the account's `codeHash`. |
| `variants` | Optional. More implementations in the same adapter, by name: other code paths the client can take (another database layout, a parallel or sparse trie, an incremental against a from-scratch computation). Each is a list of block results in the same shape as `blocks`, where every field is optional: the harness compares the fields a variant reports. A variant result may instead carry `"error": "text"` when the variant failed or found an inconsistency, such as stored trie nodes that differ from a rebuild; the harness records it as a finding. |
| `coverage` | Optional, only when the adapter was started with `BASE_DIFF_COVERAGE=1`: edge features of the adapter's trie and state database code hit while answering this request, feature name to hit count. Never compared. |

### Comparison

The harness compares every block of every adapter and every variant against the reference adapter's
`blocks`, and checks every proof against the reference state root. It compares `stateRoot`,
`storageRoots` (the key sets and the roots), each proof's `account`, each storage `value`, and the
proof node lists after dropping nodes shorter than 32 bytes other than the first (those are embedded
in their parent, and clients differ in whether they list them separately).

### Coverage

The guided fuzzer keeps a request when any coverage adapter reports an edge name no earlier request
reported; hit counts are ignored. Name edges `<client>:edge:<index>` (`nf:edge:`, `nm:edge:`;
base/base uses `edge:`), with indices stable for one build. Report only the trie, state database,
and storage engine code that a block's commit and the proofs run, not JSON parsing or the adapter
loop, and reset counters before each request. Counters must not depend on the thread a piece of
work ran on, so per-thread previous-location state must be thread-local.
