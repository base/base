---
name: system-validation-and-devnets
description: Select and operate supported integration, system-test, Docker, devnet, and load-test evidence for cross-boundary Base behavior.
---

# System Validation and Devnets

Read `AGENTS.md` and `docs/FEATURE_MAP.md` before using this skill.

## Start here

- `actions/harness`, `etc/systems`, `etc/docker`, `etc/just`, and the relevant
  runnable binary under `bin`.
- Existing focused system tests and their fixture, readiness, teardown, and
  artifact conventions before creating a new harness path.

## Choose the smallest evidence level that proves the contract

| Evidence | Write or run it when | It proves | It does not prove |
| --- | --- | --- | --- |
| **Unit test** | The behavior is local to a function, type, parser, state transition, or error mapping and can be observed through a public API without actors or processes. | A precise local invariant, boundary value, or regression in the owning crate. | Actor wiring, protocol hand-offs, persistence lifecycle, RPC transport, or process behavior. Keep it colocated in the implementation's trailing `#[cfg(test)]` module. |
| **Action test** | The contract crosses rollup protocol actors—such as L1 input, batcher, sequencer, derivation, SafeDB, Engine, unsafe gossip, or finalization—but real sockets, containers, and service loops are not the behavior under test. | Deterministic production-shaped protocol behavior through in-memory actors and production components where the harness supports them. Run `just actions test` or `cargo nextest run -p base-action-harness`. | Real Docker/service lifecycle, real RPC/provider behavior, production P2P timing, or unmodeled harness boundaries. Read [`actions/harness/README.md`](../../../actions/harness/README.md) before assuming a component is production-backed. |
| **System test** | The preserved contract crosses real processes or needs the complete L1 + L2 stack: Engine/RPC transport, node startup/shutdown, containerized dependencies, actual sequencer/validator interaction, or end-to-end operator behavior. | The public behavior of real services in the supported topology. Run `just devnet tests` or the focused `base-system-tests` target. | Behavior that only appears with a long-lived manually operated environment or an external dependency not represented by the test topology. |
| **Manual devnet check** | You need exploratory confirmation of a new operator workflow, configuration combination, timing-sensitive symptom, upgrade procedure, dashboard/log signal, or a failure that cannot yet be reproduced deterministically. | That the built local stack exhibits the observed manual scenario. Start with `just devnet up`, inspect with `just devnet status` and `just devnet logs`, and clean up with `just devnet down`. | A durable regression guarantee. Convert the confirmed scenario into a unit, action, or system test whenever the behavior can be made deterministic. |

The testing-tier commands and CI behavior are documented in
[`docs/guides/TESTING.md`](../../../docs/guides/TESTING.md). Model the processes,
configuration, topology, fixture state, and observed public contract before
choosing a tier. Treat readiness and teardown as part of the evidence, not
incidental test setup.

## Failure classification

Keep tests deterministic and isolated. Distinguish a product failure from a
harness, dependency, timeout, or readiness failure. Prefer observable readiness
or state predicates to sleeps, and retain the logs or artifacts needed to
explain a failing cross-process run.

## Validation report

Record the command, topology, fixture/configuration, intended observable
behavior, and a nonzero executed-test count. For workload or snapshot evidence,
pair this skill with `performance-evidence`.
