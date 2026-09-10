# Crate architecture

Repository-owned code is organized by responsibility. A library at
`crates/<domain>/<subsystem>/<item>` is named
`base-<domain>-<subsystem>-<item>`; two-segment library paths are also valid.
Grouping directories are not packages. Binary packages live under `bin/`,
use the `base-` prefix, and preserve their explicit executable names.

The consolidation reduced the workspace from 203 packages (83 under
`vendor/`) to 109 packages (none under `vendor/`). Retained Reth, REVM and
Alloy implementations now live with their Base subsystem. This is an
ownership change, not an upstream dependency migration. Crypto backend,
native FFI, procedural macro and proof deployment boundaries remain distinct.
Package counts do not establish a build-time improvement; no clean-build
speedup is claimed.

## Ownership

| Domain | Packages | Responsibility |
|---|---:|---|
| `common` | 20 | Cross-domain chain/configuration data, RPC and payload schemas, clients, codecs, CLI support and process utilities |
| `execution` | 33 | EVM, persistent and memory state, transaction pool, payload building, RPC, EL networking, synchronization and execution driver |
| `consensus` | 5 | CL networking, L1/L2 sources, deterministic derivation, actor service and shared consensus types |
| `batcher` | 2 | Batch/channel encoding and publication service |
| `node` | 3 | Process configuration, command parsing and fixed node assembly/lifecycle |
| `proof` | 16 | Deterministic proof execution, witness formats, submission, host/client/server and attestation boundaries |
| `infra` | 7 | Independent operational services |
| `testing` | 5 | Reusable fixtures, devnet systems and diagnostic tools |
| `bin` | 18 | Executable entry points and separate deployment artifacts |

EVM primitives and machine code sit below precompiles and runtime. Execution
blocks combine the runtime with Base block validation and execution. Concrete
inspectors remain separate so lightweight execution does not load every tracing
backend.

State types and interfaces sit below database/provider/trie implementations.
Memory state remains separate from persistent databases. Maintenance jobs own
their options and snapshot schemas; node configuration combines those options.
The MDBX sys package retains the native build/linking boundary.

RPC wire schemas live in common types. Execution handlers own simulations,
conversion and endpoint caches; the server owns transport assembly. Node service
resolves startup configuration. Consensus and execution share payload data
without sharing their implementation crates.

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
  optional, build, target-specific and dev dependencies.
- Lower domains cannot use node composition as a normal or build dependency.
  Execution integration tests may depend on node through dev dependencies.
- Production libraries cannot depend on testing helpers, with two bounded
  exceptions: node/service uses the operational debug client, and
  execution/sync/pipeline exposes optional testing/support fixtures through its
  explicit `test-utils` feature.
- The pre-existing restrictions between execution, consensus, batcher, proof
  and infra remain enforced. The complete policy is in
  [check-crate-deps.py](../../etc/scripts/ci/check-crate-deps.py).

## Internal dependency graph

[Open the complete SVG](crates.svg), or inspect the [Graphviz source](crates.dot).
The graph contains all 109 workspace packages and 768 distinct internal edges;
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
