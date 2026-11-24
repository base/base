# `base-diff-node`

`base-reth-node` with per-request edge coverage, for the coverage-guided differential fuzzers of
real nodes: JSON-RPC (`base-diff-fuzz rpc --coverage-from base`) and state (`base-diff-fuzz
state --coverage-from base`). It runs the same node as
`bin/node` (`StandardBaseRethNode` with its standard extensions) and adds one RPC method,
`fuzz_coverage`. It is a separate binary under `etc/tools` so production builds never contain
the method or the instrumentation.

## `fuzz_coverage`

`fuzz_coverage` takes no parameters and is served on every configured transport, whatever
`--http.api` lists. It returns the `SanitizerCoverage` edges hit since the previous call and zeroes
the counters:

```json
{ "counters": 412345, "hits": [[17, 1], [905, 3]] }
```

`hits` holds `[index, count]` pairs in index order; the count is the 8-bit counter, which wraps.
An index names the same edge for as long as the binary is unchanged. Counters are process-wide,
so a reading belongs to one request only when the harness sends the node one request at a time
and reads coverage after each. The edges `fuzz_coverage` itself runs after zeroing the counters
show up in every reading.

## Build

Only the crates matched by `SANCOV_CRATES` are instrumented, through the
[`sancov-rustc.sh`](../diff-adapter/sancov-rustc.sh) wrapper. Without the wrapper the binary
builds and runs, but `fuzz_coverage` returns no counters. The set below covers request handling:
JSON-RPC parsing and dispatch (`jsonrpsee`), the `eth`, `debug`, `trace`, `txpool` and `base`
handlers in reth and Base, RPC types and their serde code, tracers (`revm-inspectors`), and the
EVM and precompiles. It leaves out the Engine API, networking, sync, and storage. The chain
advancing still runs some instrumented code (the EVM, the RPC caches that follow new blocks), so
the harness discards one reading after each advance.

```sh
SANCOV_CRATES="jsonrpsee_server jsonrpsee_core jsonrpsee_types reth_rpc reth_rpc_eth_api \
  reth_rpc_eth_types reth_rpc_convert reth_rpc_server_types alloy_rpc_types_eth \
  alloy_rpc_types_trace alloy_rpc_types_debug alloy_serde revm revm_* base_common_evm \
  base_common_precompiles base_execution_evm base_execution_rpc base_metering \
  base_proofs_extension base_execution_eip8130_rpc base_txpool_rpc base_common_rpc_types" \
RUSTC_WRAPPER=etc/tools/diff-adapter/sancov-rustc.sh CARGO_TARGET_DIR=target/sancov-node \
  cargo build --release -p base-diff-node
```

Use a target directory for each `SANCOV_CRATES` value: cargo does not see the variable, so it
would reuse crates built with another set. The binary takes the same arguments as
`base-reth-node`.

For the state campaign, instrument the trie, state root, database, and persistence code instead:
reth's trie crates (including the sparse and parallel tries), the provider and database layers
(MDBX, static files, codecs), the engine tree and in-memory chain state, stages, pruning, and
Base's proofs-history storage and extension.

```sh
SANCOV_CRATES="reth_trie reth_trie_common reth_trie_db reth_trie_parallel reth_trie_sparse \
  reth_provider reth_db reth_db_api reth_db_common reth_db_models reth_storage_api \
  reth_storage_overlay reth_chain_state reth_engine_tree reth_static_file reth_nippy_jar \
  reth_libmdbx reth_prune reth_stages reth_stages_api reth_codecs alloy_trie \
  base_execution_trie base_proofs_extension" \
RUSTC_WRAPPER=etc/tools/diff-adapter/sancov-rustc.sh CARGO_TARGET_DIR=target/sancov-state \
  cargo build --release -p base-diff-node
```

Background work (persistence, proofs history) also bumps these counters between requests; the
state campaign sums every reading over an input rather than attributing them to single requests.
