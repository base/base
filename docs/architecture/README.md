# Crate architecture

Repository-owned code is organized by responsibility. A library at
`crates/<domain>/<subsystem>/<item>` is named
`base-<domain>-<subsystem>-<item>`. When a subsystem contains only one crate,
it lives directly at `crates/<domain>/<subsystem>` and is named
`base-<domain>-<subsystem>`, for example `crates/execution/txpool` and
`base-execution-txpool`. The checker rejects single-crate grouping directories.
Groups containing multiple crates, such as EVM and state, retain their children.
Grouping directories are not packages. Binary packages live under `bin/`,
use the `base-` prefix, and preserve their explicit executable names.

The workspace contains 90 packages, down from 94 after the first Base-only simplification
and 109 before it
(and 203 before the earlier vendor consolidation). Local Reth, REVM, and Alloy code
lives with its Base subsystem. Native FFI, procedural macro, and proof deployment
boundaries remain distinct. The dependency graph has 612 internal edges, down from 642 after the first pass and 768 before it.
These counts do not establish a build-time improvement.

## Ownership

| Domain | Packages | Responsibility |
|---|---:|---|
| `common` | 17 | Cross-domain chain/configuration data, RPC and payload schemas, clients, codecs, CLI support and process utilities |
| `execution` | 19 | EVM, persistent and memory state, transaction pool, payload building, RPC, EL networking, synchronization and execution driver |
| `consensus` | 5 | CL networking, L1/L2 sources, deterministic derivation, actor service and shared consensus types |
| `batcher` | 2 | Batch/channel encoding and publication service |
| `node` | 3 | Process configuration, command parsing and fixed node assembly/lifecycle |
| `proof` | 15 | Deterministic proof execution, witness formats, submission, host/client/server and attestation boundaries |
| `infra` | 7 | Independent operational services |
| `testing` | 4 | Reusable fixtures, devnet systems and diagnostic tools |
| `bin` | 18 | Executable entry points and separate deployment artifacts |

EVM primitives, interpreter, memory state, cryptographic precompiles, native precompiles,
and Base execution live in `execution/evm/runtime`. Its production context fixes Base's
transaction, configuration, and chain-state types. Database and inspector parameters remain.
Ethereum reference constructors are gated behind test utilities. RPC owns concrete inspectors,
including the retained optional JavaScript tracer.

State types and provider interfaces sit below database, provider, and trie implementations.
Database-specific traits live in the database crate. Storage codecs live with chain types;
L1 fee calculations live with chain configuration. The MDBX sys package retains its native
build/linking boundary.

RPC handlers, transport assembly, builder RPC, and metering RPC live together in
`execution/rpc`. Shared payload types also own engine data and payload builder
interfaces. The Ethereum client owns the execution-witness client shared by node service and engine observers. Discovery v5 and its wrapper share
one crate, as do wire messages and peer types.

Ethereum provider, contract, and local-node process bindings share `common/client/ethereum`.
Proof worker/service transports share `proof/client`. State pruning, maintenance, and
background tasks share `execution/state/operations`.

Consensus derivation and proof execution retain their bare-metal/no_std build
paths. Node assembly, CLI command trees and launch helpers do not belong in these
lower layers. Integration fixtures live in testing/devnet; the devnet, benchmark
and witness-diff executable launchers live in bin.

## Dependency rules

Run `just check crate-deps`. This checks package naming and placement as well
as dependency boundaries. It is included in `just check all`, `just ci` and
`just pr`. The checker has regression tests for conditional and test edges.

- Common cannot depend on execution, consensus, batcher, builder, proof, infra,
  client or node. Execution may depend on common. These domain rules include
  optional, build, target-specific and dev dependencies. Shared payload outcomes have two
  specific normal dependencies on execution state types and EVM runtime error types.
- Lower domains cannot use node composition as a normal or build dependency.
  Execution integration tests may depend on node through dev dependencies.
- Production libraries cannot depend on testing helpers, with one bounded
  exception: execution/sync exposes optional testing/support fixtures through its
  explicit `test-utils` feature.
- The pre-existing restrictions between execution, consensus, batcher, proof
  and infra remain enforced. The complete policy is in
  [check-crate-deps.py](../../etc/scripts/ci/check-crate-deps.py).

Execution also has direct dependency boundaries between subsystems:

| Consumer | Cannot depend on execution subsystems |
|---|---|
| State | Payload, RPC, txpool |
| Engine | RPC |
| EVM | Engine, network, payload, RPC, sync, txpool |
| Network | Engine, payload, RPC, sync |
| Txpool | Engine, payload, sync; RPC except in dev dependencies |
| Payload | Engine, network, RPC, sync |
| Sync | Payload, RPC, txpool |
| RPC | Engine, sync |

These rules include optional, build, target-specific, and dev dependencies,
apart from the explicit txpool/RPC integration-test allowance. Common RPC
schemas are data dependencies and are not covered by the RPC-server restriction.
The checker validates manifest edges, not transitive feature reachability.

Foundational state packages (`types`, `database`, `mdbx-sys`, `provider`, and
`trie`) also cannot depend on engine, network, or sync services, even in tests.
The state indexer's event-consumer service and operations' sync integration
tests retain their existing dependencies; they are outside this storage foundation.
Shared metrics cannot depend on chain or RPC schemas; callers supply message-size functions.

## Validation

See [build, regression-test and Docker devnet results](validation.md).

## Internal dependency graph

[Open the complete SVG](crates.svg), or inspect the [Graphviz source](crates.dot).
The graph contains all 90 workspace packages and 612 distinct internal edges;
external dependencies are excluded.

An arrow points from the consumer to its dependency. A solid line means an
unconditional normal dependency. A dashed line means the edge exists only as an
optional, target-specific, build or dev dependency. If a pair has both kinds,
the unconditional normal edge wins. Optional defaults are deliberately still
drawn dashed: this is the manifest-level graph, not one selected feature build.

Regenerate from the locked workspace metadata with:

```sh
python3 etc/scripts/local/render-crate-graph.py
```

This requires Cargo and Graphviz (`dot`). The SVG can be zoomed and searched
by crate name.
