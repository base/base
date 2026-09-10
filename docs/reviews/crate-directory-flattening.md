# Crate directory flattening

Moved 32 single nested library crates up one directory. Package names and Rust
crate imports follow their new paths; workspace dependencies, CI/build commands,
fixtures, documentation, and the dependency graph were updated together.
Multi-crate groups and binary deployment paths are unchanged.

| Previous path | Current path / package |
|---|---|
| `crates/batcher/encoding/channel` | `crates/batcher/encoding` / `base-batcher-encoding` |
| `crates/batcher/service/driver` | `crates/batcher/service` / `base-batcher-service` |
| `crates/common/cli/support` | `crates/common/cli` / `base-common-cli` |
| `crates/common/codec/macros` | `crates/common/codec` / `base-common-codec` |
| `crates/common/http/proxy` | `crates/common/http` / `base-common-http` |
| `crates/common/io/files` | `crates/common/io` / `base-common-io` |
| `crates/common/l1/transactions` | `crates/common/l1` / `base-common-l1` |
| `crates/common/process/nodes` | `crates/common/process` / `base-common-process` |
| `crates/common/runtime/tasks` | `crates/common/runtime` / `base-common-runtime` |
| `crates/consensus/batch/types` | `crates/consensus/batch` / `base-consensus-batch` |
| `crates/consensus/derive/pipeline` | `crates/consensus/derive` / `base-consensus-derive` |
| `crates/consensus/driver/service` | `crates/consensus/driver` / `base-consensus-driver` |
| `crates/consensus/network/service` | `crates/consensus/network` / `base-consensus-network` |
| `crates/consensus/source/providers` | `crates/consensus/source` / `base-consensus-source` |
| `crates/execution/payload/builder` | `crates/execution/payload` / `base-execution-payload` |
| `crates/execution/rpc/handlers` | `crates/execution/rpc` / `base-execution-rpc` |
| `crates/execution/sync/pipeline` | `crates/execution/sync` / `base-execution-sync` |
| `crates/execution/txpool/pool` | `crates/execution/txpool` / `base-execution-txpool` |
| `crates/infra/audit/service` | `crates/infra/audit` / `base-infra-audit` |
| `crates/infra/basectl/service` | `crates/infra/basectl` / `base-infra-basectl` |
| `crates/infra/shadow-metrics/service` | `crates/infra/shadow-metrics` / `base-infra-shadow-metrics` |
| `crates/infra/sidecrush/service` | `crates/infra/sidecrush` / `base-infra-sidecrush` |
| `crates/infra/snapshotter/service` | `crates/infra/snapshotter` / `base-infra-snapshotter` |
| `crates/infra/telemetry/service` | `crates/infra/telemetry` / `base-infra-telemetry` |
| `crates/infra/websocket-proxy/service` | `crates/infra/websocket-proxy` / `base-infra-websocket-proxy` |
| `crates/proof/client/providers` | `crates/proof/client` / `base-proof-client` |
| `crates/proof/execution/client` | `crates/proof/execution` / `base-proof-execution` |
| `crates/proof/host/service` | `crates/proof/host` / `base-proof-host` |
| `crates/proof/l1/submission` | `crates/proof/l1` / `base-proof-l1` |
| `crates/proof/types/protocol` | `crates/proof/types` / `base-proof-types` |
| `crates/testing/load/service` | `crates/testing/load` / `base-testing-load` |
| `crates/testing/tools/witness-diff` | `crates/testing/tools` / `base-testing-tools` |

Validation:

- Full workspace all-target check with `base-execution-rpc/js-tracer` passed.
- Bare-metal no-default-features proof check passed with the renamed
  `base-proof-execution` and `base-proof-types` packages, plus witness MPT/preimage.
- All 21 crate policy regression tests and the live workspace checker passed.
- Before/after Cargo metadata has identical dependency edges after applying the
  package rename map, including dependency kinds, optional flags, and target conditions.
- Package count remains 94; the regenerated graph has 642 internal edges.
- The checker now rejects single-crate nesting below a domain/subsystem path.

The full behavior and Docker devnet results in the architecture validation
report preceded this path-only relocation. They were not rerun for it.
