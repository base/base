# System-test to Docker-acceptance migration ledger

`migration.toml` is the authoritative ledger for the 84 distinct annotated test
identities originally present in the 22 top-level `etc/systems/tests/*.rs` files.
The historical `(source, name)` pair remains the identity even after its original
function is retired.

| Disposition | Count | Meaning |
| --- | ---: | --- |
| `ported_unverified` | 46 | An equivalent-intent PR-suite scenario and check exist; live verification is pending. |
| `moved_unit` | 3 | The parity classifier unit test moved to `acceptance/src/parity.rs`. |
| `retain_rust` | 19 | The original Rust test remains the appropriate coverage. |
| `requires_topology` | 15 | Acceptance lacks the required deployment or fault-control capability. |
| `requires_workload` | 1 | Acceptance lacks the required startup-negative workload. |

The 46 scenario-backed originals and three relocated unit tests are marked
`retired = true`. Retirement means the old function must be absent; it does not
mean the replacement scenario passed. All 46 remain `ported_unverified` until a
managed Docker run supplies evidence. Scenario and check IDs in the ledger make
that reconciliation explicit.

## Current scope and CI

The 46 replacements cover contracts, signed transactions, high-load forwarding,
seeded fuzz/sync parity, gossip-topic retirement, synchronization, fork equality,
and fork cutover. All 45 system scenario files are in the `pr` suite. Acceptance
runs for pull requests and `merge_group`; the nightly invocation uses the seeded
acceptance workload. This is broader retirement coverage, not a claim that all 84
original identities have migrated to Docker acceptance.

## Remaining coverage and blockers

The 35 original test identities that remain live comprise 19 `retain_rust`, 15
`requires_topology`, and one `requires_workload` entry:

- upgrade tests mutate a process-local registry and execution chain spec for which
  no external administrative API exists;
- snapshot prefunding is available only through the in-process snapshot launcher;
- the EIP-8130 guest proof is intentionally deferred to Everest;
- the in-process ZK startup-negative case still needs missing-session RPC behavior;
- deep L1 reorg/outage control, shadow Postgres/lifecycle deployments, snapshot
  boundaries, and managed prover/ZK hosts still need topology capabilities; and
- deterministic library, mocked Engine, wire-validation, and direct SQL contracts
  remain better expressed as Rust tests.

These are real capability gaps. Healthy heads or a mined transaction are not
substitutes for the omitted reorg, shadow, snapshot, upgrade, or prover behavior.

## Verification status

Local verification passed 107 acceptance unit tests plus the explicitly invoked
Docker Compose configuration-rendering test (no containers started), strict
acceptance Clippy, and an all-targets check of the remaining system-test crate.
All 50 scenario files validate; PR selection includes 47 scenarios (45 migrated
system scenarios plus smoke and Glamsterdam). Formatting, workflow YAML lint, and
diff whitespace checks also pass. The system-test check reports missing SP1 ELFs
and uses build-time stubs; this is not proof-runtime verification.

Live managed Docker execution remains outstanding: the matching setup and L1
images are not cached, and local disk space is insufficient for image builds.
The 46 scenario-backed entries therefore remain `ported_unverified`.

Useful non-Docker checks include:

```console
cargo run -p base-acceptance-cli -- validate acceptance/scenarios/*.toml
cargo run -p base-acceptance-cli -- select --suite pr \
  --run-id local --tested-sha "$(git rev-parse HEAD)" \
  --expected target/acceptance-expected.json \
  --matrix target/acceptance-matrix.json
```
