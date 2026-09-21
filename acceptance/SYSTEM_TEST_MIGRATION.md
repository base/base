# System-test to Docker-acceptance migration ledger

`migration.toml` is the authoritative ledger for all 84 annotated test identities
in the 22 top-level `etc/systems/tests/*.rs` files. Identity is the `(source,
name)` pair. The current dispositions are:

| Disposition | Count | Meaning |
| --- | ---: | --- |
| `ported_unverified` | 43 | Equivalent-intent implementation and scenario exist; live verification is pending. |
| `partial` | 1 | High-load forwarding is implemented, without the original rate/concurrency configuration. |
| `requires_workload` | 2 | Fuzz parity or startup-negative behavior has no acceptance workload. |
| `requires_topology` | 16 | The required deployment or fault-control capability is absent. |
| `retain_rust` | 22 | Direct database, mock/component, or deterministic unit behavior remains Rust coverage. |

Scenario existence is not proof that a case passed. Each implemented ledger entry
names the repo-relative TOML file and exact check ID so results can be reconciled
without inferring from similar names. Both smoke synchronization checks map to
`system-runtime-sync.toml`, with distinct `canonical-sync` and
`pending-flashblocks` IDs. Fork/config equality maps to the separate
`system-runtime-fork-equality-smoke.toml` lifecycle scenario.

## Implemented scope

The extended suite now contains all newly added system scenarios:

- 28 contract cases covering activation registry, B-20, policy registry, and
  transfer-policy behavior;
- 11 transaction cases, including EIP-8130 and forwarding/validity behavior;
  high-load is explicitly partial because its 40 interleaved sends do not recreate
  `max_rps=1` and `resend_after=30s`; seeded fuzz generation was removed;
- four runtime dispatch cases: full Denim cutover, full Denim/Zenith cutover,
  canonical synchronization, and pending Flashblocks synchronization; and
- generated fork equality through the acceptance provisioning lifecycle.

The PR suite remains only smoke plus Glamsterdam. The new system scenarios declare
`[ci] suite = "extended"`; the known workflow behavior is that same-repository PRs
select `pr`, while manual dispatch can select `pr`, `extended`, or `all`. This
document makes no branch-protection or required-check claim.

## Deliberate gaps and retained tests

`requires_workload` covers seeded fuzz/sustained parity and the prover startup
negative (missing-session RPC) behavior. `requires_topology` covers ZK proof hosts,
L1 outage/deep-reorg controls, snapshots, upgrade signaling, shadow Postgres and
shadow lifecycle deployments, and gossip retirement diagnostics/unsafe propagation.
In particular, no runtime dispatch implements the old shadow or gossip tests; head
health is not a substitute for those capabilities.

The 22 `retain_rust` identities are intentionally not erased from the total. They
cover mnemonic derivation and parity classification, exact mocked Engine failure
and ordering behavior, the post-Denim wire validation unit, and direct SQL schema,
reconciliation, retention, pagination, batching, constraint, and advisory-lock
contracts. A healthy Compose observation would test a different contract.

Two spike tradeoffs are visible in the implementation: the B-20 workload has a
duplicate RPC-only helper rather than sharing the old in-process harness helper,
and transaction workloads carry relatively heavy consensus/txpool wire-type
dependencies to construct the exact signed transaction forms. These choices keep
the acceptance path external-RPC-only while preserving equivalent intent; they
should be reviewed before treating the spike as permanent architecture.

## Validation commands

These commands do not contact Docker:

```console
cargo run -p base-acceptance-cli -- validate \
  acceptance/scenarios/system-transaction-eip8130-mined.toml
cargo run -p base-acceptance-cli -- plan \
  acceptance/scenarios/system-runtime-sync.toml
cargo run -p base-acceptance-cli -- select --suite extended \
  --run-id local --tested-sha "$(git rev-parse HEAD)" \
  --expected target/acceptance-expected.json \
  --matrix target/acceptance-matrix.json
```

Run one managed scenario (Docker required) with:

```console
cargo run -p base-acceptance-cli -- run \
  acceptance/scenarios/system-runtime-sync.toml \
  --output target/acceptance/system-runtime-sync
```

The ledger was checked against annotated original functions, TOML scenario/check
IDs, and duplicate identities. Parent verification completed the following:

- `cargo test --locked -p base-acceptance -p base-acceptance-cli`: 95 passed;
- `cargo clippy --locked -p base-acceptance -p base-acceptance-cli --tests -- -D warnings`:
  passed;
- CLI validation of all 48 scenario files: passed; and
- CLI `select --suite extended`: passed.

Live managed Docker execution remains unverified. The required
`devnet-setup:local-v2`, Reth, and Lighthouse images were not cached, the host had
only about 8 GiB free, and the cached `base:local` image was not proven to match
this revision. Therefore all 43 implemented entries remain `ported_unverified`;
the ledger also retains one partial high-load case, two workload gaps, 16 topology
gaps, and 22 Rust tests across the original 84 identities.
