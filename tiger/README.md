# Tiger client

Tiger is a Base execution client written in Rust without reth or revm. It validates and imports
Base blocks driven by the Base consensus node over the Engine API, keeps its own persistent state
in base-reth's datadir format, and serves the JSON-RPC methods Base nodes use. It aims to behave
exactly like base-reth for everything Base uses; [SCOPE.md](SCOPE.md) lists what is in, what is
left out, and the accepted differences. It follows
[TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md): simple
explicit code, assertions, bounded resources, and few dependencies.

Tiger is its own Cargo workspace, separate from the base workspace, so its dependency tree and
lockfile stay small. Build and test from this directory:

```sh
cargo build --release
cargo test
```

## Status

As of 2026-11-25, tiger follows the Base mainnet tip behind `base-consensus` on a base-reth
snapshot, with no state root mismatches against base-reth, and passes every mode of the
differential fuzz harness against base-reth.

## Key features

- **Every Base fork from genesis**: deposits, L1 data fee and operator fee rules, Base
  transactions, the native precompiles, and each upgrade's transitions, on
  [evm2](https://github.com/alloy-rs/evm2).
- **Engine API validation and import** driven by the Base consensus node: `newPayload`,
  `forkchoiceUpdated`, payload building from the consensus node's attributes (no mempool),
  reorgs, and EL sync from peers (discv5, RLPx, eth/69).
- **base-reth's datadir format**: MDBX, static files and RocksDB, byte-identical, so tiger opens a
  base-reth datadir or snapshot and base-reth opens tiger's full datadirs.
- **Persistent state with correct roots across restarts, crashes and reorgs**: batched
  persistence off the import path, as base-reth does; in-process healing after failed commits
  (a full disk); SIGTERM persists through the head.
- **Stateless start**: begin at a recent block with no state, filling it from an upstream's
  proofs and witnesses.
- **JSON-RPC**: the `eth_*`, `debug_*` and `trace_*` methods Base nodes serve, including proofs,
  tracing and state overrides, matching base-reth's responses.
- **Fast state root**: a parallel trie walker on threads started once, with large storages on
  jobs of their own, that skips hashed-state reads its stored branches already answer.
- **Prewarm and caches**: speculative execution and trie-path walks ahead of the serial block,
  a bounded cross-block execution cache, and a memo of signature recoveries.
- **Built-in benchmarking**: `tiger bench` replays real mainnet blocks through the validation path
  (see [Benchmarks](#benchmarks)); `TIGER_VALIDATION_LOG=1` times every block a live node imports.
- **Fault injection**: `TIGER_FAULT` crashes, tears writes or fills the disk at chosen writes,
  so the fuzz harness can test recovery.

## Results so far

**Small dependency tree.** `cargo tree` for the `tiger` binary has no `reth-*` or `revm*` crate
and 219 crates in all, against 766 for `base-reth-node`. Every direct dependency has a reason in
[Dependencies](#dependencies). About 100k lines of Rust.

**Correctness.** The differential fuzz harness (`etc/tools/diff-*`) runs base-reth as the
reference and tiger as another adapter, in every mode: block execution, chain, system config,
trie, RPC, sync and crash-recovery campaigns. Recent clean runs include 907,605 strict RPC
requests, 284k chain-fuzz blocks, 290k execution-fuzz and 241k Everest-fuzz cases, 112k mainnet
fuzz cases, sync runs with reorgs, gaps and kills, and state campaigns with injected crashes, torn
writes and full disks, all with 0 findings. Every divergence it found was fixed at its root (a
reproducer test where it would catch a regression), except the two accepted differences in
SCOPE.md. 1,258 unit and integration tests.

**Speed on Base mainnet.** Measured on one host (eagle, shared with a live base-reth) following
the tip, in clean 10-minute windows with other work paused; insert is the `newPayload` latency
the consensus node sees, p50 / p90 / p99 in ms:

| Window (UTC, 2026-11-25) | tiger insert | base-reth insert | tiger / base-reth (p50) |
| --- | --- | --- | --- |
| 05:59 (start of the day's tuning) | 44.7 / 122 / 399 | 15.9 / 44 / 129 | 2.8× |
| 08:40 | 26.3 / 89 / 344 | 17.5 / 44 / 108 | 1.5× |
| 11:32 | 22.2 / 75 / 258 | 16.5 / 47 / 139 | 1.35× |

- Execution alone is at parity with base-reth (p50 8.8 ms vs about 9).
- On the warm replay bench, validation (execution plus state root) takes p50 3.8 ms per block,
  with the state root at 1.6 ms.
- The remaining live gap is cold reads: sampled live, the validating thread waits on disk reads
  78% of its time. Most of those reads are the state root's trie and hashed-state pages; work
  continues there.
- Memory: about 1.2 GB anonymous against base-reth's 4.2 to 4.8 GB on the same host.

Profile-guided builds (`pgo.sh`) cut instructions 8.6% but showed no live gain, since live
latency is reads, not CPU (see [Optimized builds](#optimized-builds)).

## Benchmarks

`tiger bench` replays real blocks through a full node's validation path (prewarm, the cross-block
execution cache, the in-memory layer, `set_canonical`) on a datadir without persisting, checks
each state root, and prints per-phase percentiles (execution, state root, canonical update) with
blocks/s and Mgas/s. The datadir must be a full base mainnet datadir at the block before the
first replayed one; it is opened but not written, so one datadir (a ZFS clone of a snapshot, for
example) replays the same blocks again.

```sh
# The replay set: the 2,000 blocks after the datadir's tip, from any base mainnet RPC that serves
# debug_getRawBlock (base-reth does). On eagle: tip 61,858,621 (snapshot tiger-20261124).
tiger bench fetch --rpc http://127.0.0.1:8545 --from 61858622 --count 2000 --out blocks.bin
tiger bench replay --chain base --datadir <datadir> --blocks blocks.bin [--skip N] [--count N] [--no-prewarm] [--no-cache]
```

A run's numbers depend on the page cache: the first replay of a block range reads the datadir
from disk, later ones from memory. Compare builds on the same range back to back, and say which
kind of run a number comes from. `TIGER_VALIDATION_LOG=1` prints the same execution and state
root split for every block a running node validates.

`--skip N` replays N blocks before the timed ones, so two runs can measure different block
ranges. With `TIGER_BENCH_PERF_CTL=<fifo>` the replay enables and disables
`perf stat -D -1 --control fifo:<fifo>` around the timed blocks, so the counters (user cycles and
instructions, which load on a shared host disturbs far less than wall time) cover only them.

## Optimized builds

`cargo build --release` is the plain, reproducible build. Two opt-in builds do less work per
block:

- `cargo build --profile maxperf`: fat LTO and one codegen unit. Reproducible like `release`;
  the final link takes a few minutes longer.
- `./pgo.sh --datadir <datadir> --blocks blocks.bin`: profile-guided `maxperf`, trained on
  `tiger bench replay` of the first 500 blocks after the datadir's tip (`--count`, `--profile
  release`). Needs `rustup component add llvm-tools`; writes
  `target/pgo-use/<host>/maxperf/tiger`. The profile data stays under `target/`. A PGO build is
  reproducible only from the same profile, so keep the `.profdata` file next to a binary you
  deploy.

Measured on eagle (Ryzen AI Max+ 395) at 5b7a0af3c, warm, on blocks 501–1000 after the snapshot
tip while PGO trained on blocks 1–500: 11 rounds, the builds in rotated order, counters only over
the timed blocks, each build's ratio to `release` in the same round (median):

| Build | User instructions | User cycles | Validate p50 | Exec p50 |
| --- | --- | --- | --- | --- |
| `release` again (control) | 1.000 | 0.99 | 1.00 | 0.99 |
| PGO (`release`) | 0.929 | 0.94 | 0.90 | 0.89 |
| `maxperf` | 0.964 | 0.92 | 0.84 | 0.86 |
| PGO (`maxperf`) | 0.914 | 0.93 | 0.79 | 0.84 |
| `-Ctarget-cpu=native` | 0.932 | 0.96 | 0.96 | 0.96 |

User instructions vary by about ±0.5% between runs of one binary, and every optimized build ran
fewer in all 11 of 11 rounds: PGO (`maxperf`) 0.914, about 8.6% less work per block. Cycles and
wall times also came out lower in most rounds (PGO `maxperf` in 8 of 11), but eagle was shared
with other work and those differences were not significant there. Following the mainnet tip
(10-minute windows after a 20-minute warm-up, same commit, trained on older blocks), PGO
`maxperf` showed no newPayload p50 gain over `release` (29.0 vs 28.2 ms, exec 10.8 vs 10.2 ms,
within the host's noise): there, latency is mostly cold reads, not CPU. So PGO is optional and
its wall-clock gain unproven.
CPU-specific builds cut instructions but not cycles. At 14b4f55b9 (9 rounds, same method),
against `release`: `-Ctarget-cpu=native` ran 0.939 of the user instructions in 9 of 9 rounds
but 0.95 of the cycles (control 0.955); `-Ctarget-cpu=x86-64-v3 -Ctarget-feature=+pclmulqdq`
(RocksDB's build needs the second flag) 0.946 and 0.97; `CFLAGS=-march=native` for the C
dependencies alone (secp256k1, libmdbx, RocksDB; `-Ctarget-cpu` does not reach them) 0.988 and
0.98. Such a binary also runs only on CPUs like the build host's, so none is recommended. On
Zen 5, `sha3-asm` keeps keccak on its scalar code even with AVX-512 enabled. `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` (transparent huge pages
for malloc) made exec p90 slower in every round.

## Layout

| Directory | Crate | What it holds |
| --- | --- | --- |
| `evm/` | `base-tiger-evm` | Base block execution on evm2: fork schedule, transactions, fees, precompiles, system calls, upgrade transitions. |
| `store/` | `base-tiger-store` | Storage in base-reth's datadir format: MDBX (vendored libmdbx), static files, RocksDB indices; `TIGER_FAULT` crash injection. |
| `trie/` | `base-tiger-trie` | Merkle Patricia trie, state roots, proofs, and the state database over the store. |
| `precompiles/` | `base-tiger-precompiles` | Base native precompiles (activation and policy registries, B-20 tokens and factory, transaction context, nonce manager), ported from `base-common-precompiles`. |
| `precompile-storage/` | `base-tiger-precompile-storage` | Storage types, the `PrecompileStorageProvider` boundary, and the fixed gas schedule the native precompiles use. |
| `precompile-macros/` | `base-tiger-precompile-macros` | `#[contract]` and storage-layout derive macros for the native precompiles. |
| `net/` | `base-tiger-net` | Peer-to-peer networking and EL sync: discv5, RLPx, eth/69 (client and server), peers, header and body downloader. |
| `node/` | `base-tiger` | The `tiger` binary: node, Engine API, JSON-RPC, and the differential fuzz adapter. |

## Dependencies

Every direct dependency and why it is here. Anything not listed is pulled in by these.

| Dependency | Reason |
| --- | --- |
| `evm2` | The EVM interpreter, gas schedule, and standard precompiles. |
| `evm2-inspectors` | The access list inspector for `eth_createAccessList`, and the tracers for `debug_trace*` and `trace_*`. |
| `alloy-primitives` | Addresses, hashes, U256, keccak. |
| `alloy-rlp` | RLP encoding of headers, transactions, receipts and trie nodes. |
| `alloy-consensus`, `alloy-eips` | Ethereum header, transaction, receipt and EIP types that Base types build on. |
| `base-common-consensus` | Base transaction and receipt envelopes, deposits, L1 info decoding, predeploy addresses. |
| `base-common-genesis` | Base upgrade list and feature gates, rollup and system config types. |
| `base-common-l1-fees` | L1 data fee and operator fee math. |
| `base-common-flz` | FastLZ size estimate for Fjord L1 fees and the Jovian DA footprint. |
| `base-common-base-tx` | Intrinsic gas schedule shared with the Base transaction type, and the nonce manager's storage slots for `eth_getTransactionCount` with a nonce key. |
| `alloy-trie` | Trie node types, nibbles and ordered trie roots (transactions, receipts, proofs). |
| `alloy-genesis` | Parsing geth-style genesis files, the format base-reth takes as `--chain`. |
| `alloy-rpc-types-engine`, `base-common-rpc-types-engine` | Engine API payload, attribute, forkchoice and envelope wire types, and base-reth's payload id. |
| `alloy-rpc-types-eth`, `base-common-rpc-types` | JSON-RPC block, transaction and receipt shapes, so responses serialize as base-reth's do. |
| `base-proof-mpt` | Merkle Patricia trie over hash-addressed nodes, used for witness state and the node's state until `trie/` replaces it. |
| `serde`, `serde_json` | JSON-RPC and the diff adapter protocol. |
| `secp256k1` | Node keys, ECDH and signatures for RLPx, discv5 and ENRs (libsecp256k1, already in evm2's tree). |
| `aes`, `ctr` | AES-256-CTR for RLPx ECIES and frame encryption, and AES-128-CTR for discv5 packet headers. |
| `aes-gcm` | AES-128-GCM for discv5 message encryption. |
| `sha2`, `hmac` | SHA-256 and HMAC for the RLPx ECIES KDF and MAC, discv5's HKDF, and HS256 Engine API JWTs. |
| `getrandom` | Random node keys, nonces and ephemeral keys from the OS. |
| `rustls` | TLS for an `https://` stateless upstream (operators' proof endpoints are https); `ring` provider, no aws-lc. |
| `webpki-roots` | The Mozilla root certificates rustls verifies the upstream's certificate against. |
| `cc` (build) | Compiles the vendored libmdbx C sources. |
| `rocksdb` | base-reth storage v2 keeps five tables (history indices, transaction hash numbers, block access lists) in RocksDB; tiger must read and write the same files. Features `lz4`, `zstd` (reth's column family compression), `bindgen-runtime`. |
| `zstd` | base-reth's compact transaction and receipt encodings in static files are zstd-compressed with reth's dictionaries. |
| `base-common-consensus` (store) | The Base transaction and receipt types the static-file codecs encode. |
| `alloy-sol-types` | Solidity ABI encoding and decoding of the native precompile interfaces (`sol!`), and the node's decoding of revert reasons into RPC error messages as reth does. |
| `syn`, `quote`, `proc-macro2` | Parsing and generating code in `base-tiger-precompile-macros`. |
| `k256` (dev) | Signing EIP-2612 permits in the B-20 permit tests (already in the tree through `alloy-primitives/k256`). |
| `rstest`, `proptest` (dev) | Parameterized and property tests carried over unchanged from the base/base precompile crates. |

## Code copied from other projects

| Tiger path | Source | Upstream revision | Notes |
| --- | --- | --- | --- |
| `store/libmdbx/` | reth `crates/storage/libmdbx-rs/mdbx-sys/libmdbx/` (libmdbx 0.13.12, Apache-2.0) | base/reth `2b20e42de9c42b51ea0fb56599fda3c2f22640ef` | `mdbx.c`, `mdbx.h`, licenses. Patched: fault hooks around `pwrite`/`fsync` (search `tiger`). |
| `store/src/mdbx_sys.rs` | generated by reth's `reth-mdbx-sys` build script (bindgen 0.72.1) | same | Pre-generated so the build needs no libclang. |
| `store/src/mdbx.rs` | simplified from reth `crates/storage/libmdbx-rs/src/` (Apache-2.0) | same | Environment flags as reth's `DatabaseEnv::open`, without `MDBX_WRITEMAP`. |
| `store/src/static_files.rs` | hand-written after reth `crates/storage/nippy-jar/src/` and `crates/static-file/types/src/` (MIT/Apache-2.0) | same | Same file formats (`.conf` bincode, `.off`, `.csoff`), commit order and heal rules; LZ4 writes literal-only blocks. |
| `store/src/rocks.rs` | options from reth `crates/storage/provider/src/providers/rocksdb/provider.rs`; roaring treemap format of the `roaring` crate 0.11 as reth's `IntegerList` uses it | same | Hand-written roaring serializer (array and bitmap containers) and history sharding as reth's. |
| `store/src/block_codec.rs` | hand-written after reth-codecs 0.8.2 (`alloy/transaction/*`, `alloy/{signature,access_list,authorization_list,log}.rs`, the `Compact`/`CompactZstd` derives) and base/base `crates/common/consensus/src/reth_compat.rs` (MIT/Apache-2.0) | reth-codecs 0.8.2 at base/reth `2b20e42` | Byte-identical against a base-reth datadir (Base native transactions: round-trip tested only). |
| `store/zstd/*_dictionary.bin` | `reth-zstd-compressors` 0.8.2 (MIT OR Apache-2.0) | same | Unchanged. |
| `store/src/codec.rs` (header) | reth-codecs 0.8.2 `alloy/header.rs` | same | Hand-written, byte-identical against a base-reth datadir. |
| `store/src/datadir.rs` | commit order and heal after reth's static file and RocksDB consistency checks | same | Hand-written. |
| `store/build.rs` | reth `crates/storage/libmdbx-rs/mdbx-sys/build.rs` | same | Without bindgen. |
| `precompiles/` | base/base `crates/common/precompiles/` (MIT) | base/base `3001b65c30e2ebb4e28bb19242ceade6bc604e4b` | Without revm adapters, metrics, observers and Ethereum precompile caps; dispatch through `NativePrecompiles::run`. |
| `precompile-storage/` | base/base `crates/common/precompile-storage/` (MIT) | same | Own `Bytecode`, `Checkpoint` and fixed `GasSchedule` instead of revm types; the evm2 provider lives in `evm/src/native_precompiles.rs`. |
| `precompile-macros/` | base/base `crates/common/precompile-macros/` (MIT) | same | Unchanged except the `Bytecode` path. |
