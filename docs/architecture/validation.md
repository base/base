# Consolidation validation

## Second Base-only simplification pass

Validated on 2026-09-10. All 50 suggestions have separate commits, listed in the
[implementation checklist](../reviews/next-50-simplifications.md).
The workspace now has **90 packages and 612 internal dependency edges**, down
from 94 and 642 at the start of this pass.

### Build and behavior checks

- The complete workspace passes `cargo check --workspace --all-targets --features
  base-execution-rpc/js-tracer --offline`.
- The production node passes `cargo check -p base-bin-base --locked --offline`
  without test utilities. The four proof libraries pass the bare-metal command below.
- The crate dependency checker and all **22** policy regression tests pass.
- Documentation tests for the Ethereum client, EVM runtime/blocks, RPC, txpool, and
  network service pass: **77 examples passed, 39 ignored**. Public examples now
  use the consolidated crate paths and concrete Base constructors.
- `cargo +nightly-2026-09-01 fmt --all -- --check` and `git diff --check` pass.
- The expanded nextest selection exercises **7,343 distinct tests** across 25
  affected crates, with **21 skipped**. The main run passed 7,342 cases and exposed
  a stack overflow in one EIP-8130 fixture under the combined feature set. Boxing
  the fixture's node-startup future fixes that failure; all 13 EIP-8130 tests then
  passed with the same 25-package feature graph. No production stack limit was changed.
- This selection covers the client/contract/process merge, proof clients, state
  operations, EVM and JS tracing, RPC servers, pool validation/forwarding, network
  requests/gossip/snap proofs, engine persistence, node lifecycle, and proof execution.
- Compiler documentation/import warnings and upstream future-compatibility notices
  remain. This is not a warning-free Clippy audit.

The regression command selects the original 17 packages listed below plus
`base-common-client-ethereum`, `base-proof-client`, `base-execution-state-operations`,
`base-common-observability-metrics`, `base-execution-engine-observers`,
`base-execution-network-service`, `base-node-service`, and `base-node-cli`, using
`cargo nextest run --tests --all-features --offline --no-fail-fast --test-threads 8`.

### Fresh HA devnet and persistence

The standard HA devnet was rebuilt with Rust 1.96.0 and fresh chain data. After
validation fixes, the images were rebuilt again and all six nodes were recreated
against their existing data. The final Base image is:

`sha256:522aa7f22449e00613f63292c49a8bbed3efb9919f85fa4c40488c10241896be`

L1 value and blob transactions succeeded (blob gas `0x20000`), and the portal,
system config, and L1 bridge contracts were deployed. An L2 value transfer through
the client RPC succeeded in block **345**:

`0x4112f5197b8c221af1f7798822e191002d5919496837a357f5ef74711d934a90`

All six nodes returned the same successful receipt and canonical block hash,
advanced their safe head beyond the transfer, and retained the receipt after
restart. The RPC call tracer returned a successful CALL using 21,000 gas before
and after restart. The initial stock smoke script printed an empty client receipt
once during propagation; a subsequent direct submission returned the receipt
that was independently checked across all nodes.

| Node | Unsafe head after restart | Safe head after restart |
|---|---:|---:|
| builder | 2901 | 2802 |
| client | 2901 | 1904 |
| rpc | 2901 | 1904 |
| sequencer-1 | 2901 | 2802 |
| sequencer-2 | 2901 | 2802 |
| shadow-validator | 2901 | 1901 |

Node, bootnode, and batcher log scans before and after recreation found no
ERROR/FATAL/panic entries. The devnet is left running. These checks validate the
local HA topology and smoke workload, not every deployment or feature combination.

## Crate directory flattening

The subsequent [32-crate path flattening](../reviews/crate-directory-flattening.md)
passes the workspace all-target JS-enabled build, bare-metal proof check, and
21 policy tests. Dependency edges and feature/target flags are unchanged.
Commands below use current package names; the behavior tests and Docker run
preceded that path-only relocation.

## Base-only simplification

Validated on 2026-09-10. Implements suggestions 1 and 3–50; JavaScript tracing
was explicitly retained. See the [implementation checklist](../reviews/base-only-simplification.md).

- The workspace has **94 packages and 642 internal edges**, down from 109 and 768.
- The full workspace passes `cargo check --workspace --all-targets --features
  base-execution-rpc/js-tracer --offline`, including benchmarks.
- The production node passes `cargo check -p base-bin-base --locked --offline`
  without enabling test utilities.
- **5,607 unit and integration tests passed, with 9 skipped**, across 17 affected
  crates. This includes EVM/precompile golden tests, JS tracer tests, RPC server
  integration, txpool validity forwarding, engine persistence, database/provider
  behavior, payload building, chain types, discovery/wire, CLI and proof execution.
- The crate policy checker and all **21** policy regression tests pass, including
  execution-subsystem, foundational-storage, and single-crate nesting restrictions.
- `cargo +nightly-2026-09-01 fmt --all -- --check` and `git diff --check` pass.
- The proof execution client, witness MPT, witness preimage, and protocol types
  pass the bare-metal check below.
- Compiler warnings remain, including documentation/import warnings and upstream
  future-compatibility notices. These checks are not a warning-free Clippy audit.
- A fresh Docker devnet build and end-to-end validation also passed, including
  RPC-node restart recovery; see the current run below.

The bare-metal check verifies that merging execution components has not forced
`std` into the proof libraries:

```sh
cargo check -p base-proof-execution -p base-proof-witness-mpt \
  -p base-proof-witness-preimage -p base-proof-types \
  --no-default-features --target riscv32imac-unknown-none-elf --offline \
  --config 'build.rustflags=["--cfg", "getrandom_backend=\"custom\""]'
```

The behavior test selection was:

```sh
cargo nextest run --tests \
  -p base-execution-evm-runtime -p base-execution-evm-macros \
  -p base-execution-evm-blocks -p base-execution-txpool \
  -p base-execution-rpc -p base-execution-payload \
  -p base-execution-engine-driver -p base-execution-state-types \
  -p base-execution-state-database -p base-execution-state-provider \
  -p base-common-types-chain -p base-common-chain-config \
  -p base-common-types-payload -p base-execution-network-discovery \
  -p base-execution-network-wire -p base-node-config -p base-proof-execution \
  --features base-execution-rpc/js-tracer,base-execution-evm-runtime/test-utils,base-execution-evm-blocks/test-utils,base-execution-txpool/test-utils \
  --offline --no-fail-fast --test-threads 8
```

## Fresh Docker devnet after Base-only simplification

On 2026-09-10, the standard HA devnet was rebuilt from the modified workspace
with Rust 1.96.0 and started from fresh chain data. This environment required
`sg docker -c 'just devnet up'` to pick up the account's existing Docker group.
The resulting Base image was `sha256:31f336f69125d16444ce33454bdede53d6fd184ed7c74f501a156cedd5a93554`.

`just devnet smoke` passed its L1 value transfer, L1 blob transaction,
contract-deployment checks, and L2 transfers through both builder and client.
The optional ingress stack was not started, so its smoke check was skipped.

The client-submitted transaction was
`0x4112f5197b8c221af1f7798822e191002d5919496837a357f5ef74711d934a90`,
in L2 block **46**, hash
`0x3fb7aed444bc42f6986d6af362aacf23693e9cb090b84f86d2c60025ad1ceac3`.
All six nodes returned the same successful receipt and canonical block hash,
and all six advanced their safe heads beyond that transaction. Each node had
five peers. Initial unsafe heads were 20; follower safe heads were zero.
The configured 15-L1-block confirmation delay was preserved.

`debug_traceTransaction` with `callTracer` succeeded through the RPC node.
After the transaction became safe everywhere, `docker restart base-rpc`
verified storage reopening: the RPC node retained the receipt and canonical
block, recovered its peers, and caught up with the other nodes.

| Node | Unsafe head after restart | Safe head after restart |
|---|---:|---:|
| builder | 1316 | 1243 |
| client | 1316 | 342 |
| rpc | 1316 | 342 |
| sequencer-1 | 1316 | 1243 |
| sequencer-2 | 1316 | 1243 |
| shadow-validator | 1316 | 342 |

The six nodes, bootnode, L1 services, and all three HA conductors were healthy;
both batchers were running. The node/bootnode/batcher log scan found no
ERROR/FATAL/panic entries except one RPC-node receiver-closed error immediately
after the deliberate SIGTERM. The RPC node then shut down and restarted
successfully, with no subsequent errors in the reviewed log interval.

The devnet was left running for further inspection. These results cover this
local topology and smoke workload, not every deployment or feature combination.

## Historical consolidation at `fd23be78c`

The following results describe the earlier 109-crate layout. They do not
validate the subsequent Base-only simplification.

Validated on 2026-09-10. The workspace layout and ownership are described in
[the architecture overview](README.md).

### Earlier build and policy checks

- `cargo check --workspace --all-targets` passed after the final package moves.
- `just check crate-deps` passed, including nine policy regression tests.
- The proof execution client, witness MPT, witness preimage and protocol types
  passed a no-default-features check for
  `riscv32imac-unknown-none-elf` with the custom getrandom backend.
- Documentation warnings remain excluded from this cleanup. Cargo also reports
  upstream future-compatibility notices for proc-macro-error2 and russh.
- The generated SVG was parsed successfully and contains 109 package nodes,
  nine ownership clusters and 768 internal edges.

Each merge was validated separately before its commit. Checks included the
affected EVM/crypto backends, provider/trie/database behavior, RPC and CLI
interfaces, pure proof targets, and real Postgres, S3/MinIO and Docker system
fixtures where relevant. The last block API specialization passed 379 chain
type tests and 285 provider tests. Binary naming changes passed 57 tests, with
one pre-existing ignored test. The extracted witness-diff binary builds and
prints its existing CLI help.

The final block-consumer regression run passed 165 execution-driver tests,
518 transaction-pool tests and 75 node-service tests (758 total). Three
transaction-pool tests remain ignored.

### Earlier Docker devnet

`just devnet up` rebuilt the Base image using Rust 1.96.0 and started the
complete Docker stack. The Base image ID was
`sha256:0bcd27b20b3d439e29352c9141cd833afb26e7840533bd6e51c655ac7e6a0b28`.

A value transfer submitted through the client RPC succeeded in block 17:

`0x49a7f1a4aa93da00e7250fb8d314f755476d33510d47f37ae31b8595405a0bcf`

All six nodes returned the same block hash and successful receipt, and all six
advanced their safe head past that transaction. Initial unsafe heads were 15
and initial safe heads were zero.

| Node | Observed unsafe head | Observed safe head |
|---|---:|---:|
| Builder | 1074 | 1002 |
| Client | 1074 | 102 |
| RPC | 1074 | 42 |
| Sequencer 1 | 1074 | 1002 |
| Sequencer 2 | 1074 | 1002 |
| Shadow validator | 1074 | 99 |

Follower safe heads lag because their configured L1 confirmation delay is
preserved. The six nodes, both batchers and bootnode had no ERROR/FATAL/panic
log entries during the run. Startup warnings about empty peer caches,
ephemeral peer keys and conductor readiness were present; the conductor
recovered and block production continued. Discovery also reports an empty
closest-peer set in this isolated devnet.

At the end of that earlier run, the devnet remained running. These observations validate this run, not every
possible deployment or runtime feature combination.
