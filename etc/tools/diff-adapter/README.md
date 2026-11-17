# `base-diff-adapter`

The base/base adapter for the differential execution fuzz harness (`base-diff-fuzz`). It
answers the newline-delimited JSON protocol in [`../diff-fuzz/PROTOCOL.md`](../diff-fuzz/PROTOCOL.md)
on stdin/stdout and is the reference implementation other execution clients are compared against,
so it runs the same code the node (`base-reth-node --chain base`) runs rather than a copy.

## What it runs

- **Chain:** `BaseChainSpec::parse_chain("base")`, the spec the node builds for `--chain base`.
- **Build mode** follows the payload builder: the EVM env comes from
  `BaseEvmEnvBuilder::next_evm_env` (parent plus attributes, with the request's base fee), the
  block runs through reth's `BlockBuilder` from `BaseEvmConfig::create_block_builder`
  (system calls, deposits, fee distribution, post-block changes), and a transaction whose
  execution returns a validation error is skipped and reported in `rejected`.
  `builder.finish` computes the state root and assembles the header with `BaseBlockAssembler`.
  Build mode checks no header or ordering rules.
- **Validate mode** follows the engine tree's new-payload path: the block becomes an
  `ExecutionData` payload and goes through `BaseEngineValidator::convert_payload_to_block`
  (block hash check), `BaseBeaconConsensus` header, header-against-parent, and pre-execution
  checks, execution with the env and context from the payload (`ConfigureEngineEvm`) and reth's
  block executor, `validate_block_post_execution`, the engine validator's hashed-state checks
  (Isthmus withdrawals root, `BaseTime` progression), and the state root check.
- **Pre-state** is a `WitnessState`, a reth `StateProvider` over the request witness. Reads walk
  the account and storage tries from the parent state root with `base-proof-mpt`, the fault
  proof's trie. Roots come from applying the hashed post state to the partial trie. A read
  that leaves the witness (a trie node, bytecode, or ancestor hash it does not have) makes the
  response `ok: false` with an error naming what was missing.

## Usage

```sh
cargo build --release -p base-diff-adapter
target/release/base-diff-adapter < requests.jsonl > responses.jsonl
```

Logs go to stderr; set `RUST_LOG` to change the level (default `warn`).

## Coverage

With `BASE_DIFF_COVERAGE=1` in its environment, the adapter runs build-mode blocks with a revm
inspector (`Coverage`) and adds a `coverage` object to each response: semantic features such as
opcode by fork and call depth, gas charged per instruction, how each frame ended, call, create,
and precompile outcomes, native precompile selectors and errors, `SELFDESTRUCT` kinds, and
transaction types by outcome and rejection kind (see "Optional coverage" in `PROTOCOL.md`).
Compared fields are unchanged. The per-instruction cost is two counter increments.

### Edge coverage

The `edge-coverage` feature adds `edge:<index>` features from `SanitizerCoverage` inline 8-bit
counters in the EVM crates only (revm, `base-common-evm`, and `base-common-precompiles`). The
wrapper script instruments those crates and leaves the rest of the build unchanged; use a
separate target directory, since the instrumented crates invalidate every crate built on them:

```sh
RUSTC_WRAPPER=etc/tools/diff-adapter/sancov-rustc.sh CARGO_TARGET_DIR=target/sancov \
  cargo build --release -p base-diff-adapter --features edge-coverage
```

The instrumented build registers about 26,000 counters and costs about
5% per request with coverage enabled. `-C instrument-coverage` is not used: it instruments the
whole binary and writes its profile only at process exit, so it cannot report per request.

