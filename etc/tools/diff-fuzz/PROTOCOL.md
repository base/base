# Differential execution protocol

This document defines how the differential fuzz harness (`base-diff-fuzz`) talks to an
execution client adapter. Every client under test (base/base, needlefish, Nethermind) ships
one adapter binary that speaks this protocol. The harness sends the same block to every
adapter and compares what they compute.

One protocol covers both workloads:

- **build** mode: fuzzed blocks. The harness supplies a block template; the adapter executes
  it, skips transactions that are invalid, and reports every header field it would commit to.
- **validate** mode: real Base mainnet blocks. The adapter executes the canonical block on its
  normal block-import path, every transaction must be valid, and the adapter reports the same
  computed fields plus whether the block passes validation against its canonical header.
- **systemConfig** mode: L1 `ConfigUpdate` logs. The adapter applies the receipts of an L1
  block to a system config as its derivation does and reports the resulting config and what it
  did with every log (see "System config mode").

Pre-state is always a stateless witness (trie nodes and bytecodes) rooted at the parent's
state root. For fuzz cases the harness builds a full trie of a synthetic genesis allocation,
so the witness is complete; for mainnet blocks it is the `debug_executionWitness` of the block.

## Transport

- The harness starts the adapter once and keeps it running.
- It writes one request per line (newline-delimited JSON, UTF-8) to the adapter's stdin.
- The adapter writes exactly one response line to stdout per request, in request order, and
  flushes after every line. Nothing else goes to stdout. Logs go to stderr.
- The adapter exits with status 0 when stdin closes.
- A request the adapter cannot process at all (unparseable JSON, a witness node it needs but
  does not have, an internal panic it can catch) still gets a response, with `"ok": false`
  and `error` set. An adapter crash is reported by the harness as a crash for that case.
- Requests are independent: an adapter keeps no state between requests other than caches
  whose contents cannot change results.

## Encodings

- Quantities (`number`, `timestamp`, `gasLimit`, balances, nonces, ...) are `0x`-prefixed
  hex strings without leading zeros (`"0x0"` for zero), as in Ethereum JSON-RPC.
- Fixed-size data (hashes, addresses, bloom, storage keys and values) are `0x`-prefixed
  lower-case hex of the exact width (storage keys and values are 32 bytes).
- Variable data (transactions, headers, trie nodes, code, extra data, log data) are
  `0x`-prefixed hex.
- Headers are RLP-encoded Base headers (the same bytes that hash to the block hash).
- Transactions are EIP-2718 encodings as they appear in a block body: legacy transactions as
  RLP lists, typed transactions as `type || payload`, deposits as `0x7e || rlp(...)`.

## Request

```json
{
  "id": "fuzz-000123",
  "chain": "base-mainnet",
  "mode": "build",
  "ancestors": ["0x<rlp parent header>", "0x<rlp grandparent header>", "..."],
  "header": {
    "number": "0x...",
    "timestamp": "0x...",
    "coinbase": "0x4200000000000000000000000000000000000011",
    "prevRandao": "0x...",
    "gasLimit": "0x...",
    "baseFeePerGas": "0x...",
    "extraData": "0x...",
    "parentBeaconBlockRoot": "0x...",
    "stateRoot": "0x...",
    "receiptsRoot": "0x...",
    "transactionsRoot": "0x...",
    "logsBloom": "0x...",
    "gasUsed": "0x...",
    "blobGasUsed": "0x...",
    "excessBlobGas": "0x...",
    "withdrawalsRoot": "0x...",
    "requestsHash": "0x...",
    "hash": "0x..."
  },
  "transactions": ["0x7ef8...", "0x02f8..."],
  "witness": {
    "state": ["0x<rlp trie node>", "..."],
    "codes": ["0x<bytecode>", "..."]
  }
}
```

| Field | Meaning |
| --- | --- |
| `id` | Opaque case identifier, echoed in the response. |
| `chain` | Chain configuration. `"base-mainnet"` (chain 8453) is the only value the harness sends today. Fork activation, the subsecond block timestamp schedule, predeploys, and every other chain parameter come from the client's own Base mainnet configuration. The fork in effect is decided by `number` and `timestamp` exactly as on mainnet. |
| `mode` | `"build"`, `"validate"`, or `"systemConfig"` (see "System config mode"). |
| `ancestors` | RLP headers, newest first. `ancestors[0]` is the parent. At most 256 entries; the adapter serves `BLOCKHASH` from them and uses the parent for everything a block derives from its parent (base fee, blob fields, the parent state root that the witness is rooted at, the EIP-2935 and EIP-4788 system calls). |
| `header` | The block's own header fields. In **build** mode only the inputs are present: `number`, `timestamp`, `coinbase`, `prevRandao`, `gasLimit`, `baseFeePerGas`, `extraData`, `parentBeaconBlockRoot` (and `excessBlobGas` when the fork has it). In **validate** mode every field of the canonical header is present, including the results and `hash`. |
| `transactions` | The block body, in order. In build mode the harness puts the L1 attributes deposit first (and from Denim the `BaseTime` deposit second), exactly as the sequencer does; the adapter does not add or reorder transactions. |
| `witness.state` | Trie node preimages for the parent state: account trie and storage trie nodes, in any order, possibly with extras. |
| `witness.codes` | Contract bytecodes the block may load, in any order, possibly with extras. |

### Build mode semantics

The adapter executes the block on the same code path its node uses to build or import a block
(block executor, transaction processor, system calls, fee distribution, post-block state
commit), with one difference: a transaction that fails validity checks (bad nonce, bad
signature, insufficient balance, gas limit above the block's remaining gas or above the
per-transaction cap, unsupported type, and so on) is **skipped** without changing state, and
reported in `rejected`. A transaction that is valid but reverts or runs out of gas is
included with `status` 0x0, as in any block.

Build mode only builds. It does not check header-level or ordering rules (the timestamp
schedule, the `BaseTime` deposit's position or milliseconds, extra data well-formedness,
deposits-first ordering) and never reports `valid: false` for them; the harness is responsible
for well-formed blocks. Execution still uses what the block contains: the `BaseTime` deposit
runs as an ordinary deposit, and extra data feeds `computedBaseFeePerGas`. Synthetic chains use
the client's own mainnet configuration, so the Beryl activation admin is the mainnet
chain-config address (the harness acts as it through deposits), and from Cobalt the admin the
genesis state stores takes precedence, as on mainnet.

Use the `baseFeePerGas` given in the header for execution, even if it differs from what the
parent implies. Report the base fee the parent implies in `computedBaseFeePerGas` so that base
fee rules are compared independently.

Header fields the adapter computes in build mode are the fields the client would write into
the header of a block containing exactly the included transactions.

### Validate mode semantics

The adapter imports the canonical block on its normal block-validation path: pre-execution
header checks against the parent, execution, and post-execution checks against the canonical
header. Report the computed fields regardless of whether validation passes. `valid` is
`true` only when the client would accept the block as canonical. `rejected` must be empty in
validate mode; a transaction that fails validity checks makes the block invalid.

## Response

```json
{
  "id": "fuzz-000123",
  "ok": true,
  "valid": true,
  "invalidReason": null,
  "rejected": [{ "index": 3, "error": "nonce too low" }],
  "header": {
    "stateRoot": "0x...",
    "receiptsRoot": "0x...",
    "transactionsRoot": "0x...",
    "logsBloom": "0x...",
    "gasUsed": "0x...",
    "blobGasUsed": "0x...",
    "withdrawalsRoot": "0x...",
    "requestsHash": "0x...",
    "hash": "0x..."
  },
  "computedBaseFeePerGas": "0x...",
  "receipts": [
    {
      "type": "0x2",
      "status": "0x1",
      "cumulativeGasUsed": "0x...",
      "logs": [{ "address": "0x...", "topics": ["0x..."], "data": "0x..." }],
      "depositNonce": "0x...",
      "depositReceiptVersion": "0x1"
    }
  ],
  "post": {
    "0x<address>": {
      "nonce": "0x1",
      "balance": "0x...",
      "codeHash": "0x...",
      "storage": { "0x<slot>": "0x<value>" }
    },
    "0x<deleted address>": null
  }
}
```

| Field | Meaning |
| --- | --- |
| `ok` | `false` only for adapter-level failures (see Transport). When `false`, only `id` and `error` are required. |
| `error` | Adapter failure description when `ok` is `false`. |
| `valid` | Validate mode: whether the block passes the client's validation. Build mode: always `true` unless the block as a whole cannot be built (for example, a malformed L1 attributes deposit), in which case `false` with `invalidReason`. |
| `invalidReason` | Free text, not compared. |
| `rejected` | Build mode: indices (into `transactions`) of skipped transactions, ascending, with free-text errors. Indices are compared, error texts are not. |
| `header` | Computed header fields. `withdrawalsRoot`, `requestsHash`, and `blobGasUsed` are present exactly when the fork puts them in the header. `hash` is the hash of the full header assembled from the request inputs and the computed fields (in build mode, `header` inputs plus computed fields, with `parentHash = keccak(ancestors[0])`, `ommersHash` empty-list hash, `difficulty` 0, `nonce` 0, `mixHash = prevRandao`, `withdrawalsRoot` as the fork requires). |
| `computedBaseFeePerGas` | The base fee the client derives for this block from the parent header and chain rules. |
| `receipts` | One per included transaction, in block order. `depositNonce` and `depositReceiptVersion` only on deposit receipts, exactly when the fork stores them in the receipt. |
| `post` | Every account whose nonce, balance, code hash, or storage changed in this block, with its values after the block, and only the storage slots whose value changed (new value, `0x00..00` when cleared). `wiped: true` marks an account that existed before the block, was destroyed during it (a pre-Cancun `SELFDESTRUCT`, or an EIP-161 removal), and exists after it again (recreated by a `CREATE2`, a value transfer, or a later transaction): its storage starts from empty, so every parent slot not listed in `storage` is zero. Omit it otherwise. An account that existed before and does not exist after is `null`. An account that did not exist before and does not exist after is omitted, even if it was touched. Keys are lower-case. |
| `coverage` | Optional, never compared. Coverage features of the execution, feature name to hit count (see "Optional coverage"). |

## Comparison rules

The harness considers two adapters to agree on a case when all of these match exactly:
`ok`, `valid`, rejected indices, every `header` field, `computedBaseFeePerGas`, every receipt
field, and `post`. Any other response field, such as `coverage`, is ignored. A case where any
adapter returns `ok: false` is reported separately (as a harness or adapter bug, not a consensus
difference) and never counted as agreement.

For validate mode the harness additionally checks each adapter against the canonical header:
computed `stateRoot`, `receiptsRoot`, `logsBloom`, `gasUsed`, `blobGasUsed`,
`withdrawalsRoot`, `requestsHash`, and `hash` must equal the canonical values, and `valid`
must be `true`.

## System config mode

A request with `"mode": "systemConfig"` compares how clients derive the L1 system config from
`ConfigUpdate` logs. It carries no block: `ancestors`, `header`, `transactions`, and `witness`
are absent. It gives a system config, the receipts of one L1 block, and the L1 block's
timestamp; the adapter applies the receipts to the config on the code path its derivation uses
on the receipts of each new L1 origin (base/base: `SystemConfig::update_with_receipts`), and
reports the resulting config and what it did with every log.

```json
{
  "id": "sysconfig-1-17",
  "chain": "base-mainnet",
  "mode": "systemConfig",
  "systemConfig": {
    "l1Timestamp": "0x65f23e8b",
    "forks": ["regolith", "canyon", "ecotone"],
    "config": {
      "batcherAddress": "0x5050505050505050505050505050505050505050",
      "overhead": "0x00000000000000000000000000000000000000000000000000000000000000bc",
      "scalar": "0x010000000000000000000000000000000000000000000000000c5fc500000558",
      "gasLimit": "0x3938700",
      "eip1559Denominator": "0xfa",
      "eip1559Elasticity": "0x6",
      "operatorFeeScalar": "0x0",
      "operatorFeeConstant": "0x0",
      "minBaseFee": "0x0",
      "daFootprintGasScalar": "0x190"
    },
    "receipts": [
      { "status": "0x1", "logs": [{ "address": "0x73a7...4072", "topics": ["0x1d2b...53be", "0x00..00", "0x00..02"], "data": "0x...20...20...03938700" }] }
    ]
  }
}
```

| Field | Meaning |
| --- | --- |
| `l1Timestamp` | Timestamp of the L1 block. The client decides which forks are active from it with its own Base mainnet rollup configuration, as its derivation does with the L1 origin's timestamp. On Base the only fork that changes parsing is Ecotone (`BlobAwareL1Fees` active at `l1Timestamp`): gas config updates zero the overhead and ignore an invalid scalar. |
| `forks` | The Base upgrades active at `l1Timestamp` on mainnet, oldest first, from `regolith`, `canyon`, `ecotone`, `fjord`, `granite`, `holocene`, `isthmus`, `jovian`, `azul`, `beryl`, `cobalt`, `denim`. Derived from `l1Timestamp`, for adapters whose parser takes fork flags; it never contradicts `l1Timestamp`. |
| `config` | The system config before the block. Every field is present: `batcherAddress`, `overhead` and `scalar` (32-byte words), and quantities `gasLimit`, `eip1559Denominator`, `eip1559Elasticity`, `operatorFeeScalar`, `operatorFeeConstant`, `minBaseFee`, `daFootprintGasScalar`. Values are arbitrary within their types, including zero. |
| `receipts` | The L1 block's receipts in block order: `status` (`0x1` success, `0x0` failure) and `logs` (`address`, `topics`, `data`). The L1 system config address is Base mainnet's, `0x73a79fab69143498ed3712e519a88a918e1f4072`. |

The response has `ok`, `valid: true`, and a `systemConfig` object instead of block results:

```json
{
  "id": "sysconfig-1-17",
  "ok": true,
  "valid": true,
  "systemConfig": {
    "config": { "batcherAddress": "0x...", "gasLimit": "0x3938700", "...": "..." },
    "logs": [
      { "outcome": "applied", "kind": "0x2" },
      { "outcome": "rejected", "error": "invalid data pointer" },
      { "outcome": "ignored" }
    ]
  }
}
```

| Field | Meaning |
| --- | --- |
| `config` | The system config after every log, with the same fields as the request's. Omitted when the client halted. |
| `logs` | One entry per log of every receipt, in order (receipt 0's logs, then receipt 1's, ...). `outcome` is `applied` (a well-formed update of a known kind, with `kind` its number, even if it leaves the config unchanged: an unsafe block signer update, or an Ecotone gas config update whose scalar is invalid), `rejected` (a log the client treats as a config update and skips as malformed), `ignored` (not a config update: the receipt failed, the emitter is not the system config address, or the log has no topics or a first topic other than the `ConfigUpdate` event), or `halted` (the client's derivation stops at this log, for example by throwing, instead of skipping it). After a `halted` entry the list ends. `error` is free text for `rejected` and `halted`, not compared. |

Compared fields: `ok`, `valid`, every `config` field (or its absence), the number of `logs`, and
each log's `outcome` with its `kind`. For each kind the update decodes as on L1: `data` is the
ABI encoding of `bytes`, a pointer word (32), a length word (32, or 64 for gas config), and the
payload words: 0 batcher address, 1 overhead and scalar, 2 gas limit, 3 unsafe block signer, 4
EIP-1559 params (`denominator << 32 | elasticity` as a `uint64`), 5 operator fee params (scalar
in bytes 20..24, constant in bytes 24..32), 6 minimum base fee (`uint64`), 7 DA footprint gas
scalar (`uint16`, zero meaning the default 400). `topics[1]` is the version (zero) and
`topics[2]` the kind. The harness sends both well-formed logs and logs broken in every field;
whether each is applied, rejected, or ignored is exactly what this mode compares.

## Optional tracing

When the request has `"trace": "<dir>"`, the adapter writes an EIP-3155 trace for each
transaction to `<dir>/<id>-<txIndex>.jsonl`. Tracing is for diagnosing a difference; it must
not change results, and an adapter may ignore the field.

## Optional coverage

An adapter may add a `coverage` object to build-mode responses: feature names mapped to how many
times the execution hit them. The coverage-guided fuzzer (`base-diff-fuzz fuzz`) reads it from the
reference adapter only and keeps cases that hit a feature, or a feature hit-count bucket, that no
earlier case did. Coverage must not change any compared field, and other adapters omit it.

The harness asks for coverage by starting the reference adapter with the environment variable
`BASE_DIFF_COVERAGE=1` rather than through a request field, so requests stay identical for every
adapter. `base-diff-adapter` then runs the block with a revm inspector and reports semantic
features (see `Coverage` in `diff-adapter/src/coverage.rs`), for example:

| Feature | Meaning |
| --- | --- |
| `op:<fork>:<opcode>:d<depth>` | Opcode executed under a fork at a call depth bucket (0, 1, 2-7, 8+). |
| `gas:<opcode>:<cost>` | Gas one instruction charged (exact up to 32, then a power of two): cold and warm access, `SSTORE` cases, memory expansion. |
| `end:<opcode>:<result>` | The last opcode a call frame ran and how it ended (stop, revert, or halt reason). |
| `call:<scheme>:<result>:d<depth>` | Call outcomes, with `:value` and `:nocode` markers. |
| `create:<scheme>:<result>:d<depth>:init<len>` | `CREATE` and `CREATE2` outcomes. |
| `pc:<address>:len<len>:<result>` | Standard precompile calls by input length bucket and result. |
| `np:<precompile>:<selector>:<result>[:<error>]` | Base native precompile calls by selector, result, and revert kind (unknown selectors collapse to `*`). |
| `sd:<beneficiary>:<value>:<age>` | `SELFDESTRUCT` kinds. |
| `top:t<type>:<result>`, `refund:<kind>` | Transaction outcome by type, and whether the refund was capped. |
| `tx:t<type>:s<status>`, `reject:t<type>:<error kind>` | Included transactions by type and status; rejected transactions by type and error text with numbers elided. |
| `edge:<index>` | Edges of the EVM crates, only in the `edge-coverage` build. |

