# Deferred root benchmark follow-up

Five source-supported pre-review defects were fixed after 8f84642a1: sidecar-aware advertised payload identity, canonical native receipts for EL-only blocks, explicit root-worker health/join observation, retained typed Engine responses on later failure, and external executable-to-source manifest binding. Runner changes only expose the existing optional shared service on the launch handle; no root algorithm/admission changes.

Additional regression tests were written before fixes. The command `cargo test --locked -p base-system-tests --no-default-features --lib root_bench::tests --no-run` failed in the unchanged sqlx-macros dependency with `mis-aligned LINKEDIT string pool, fileOffset=0x0066F2E4`. Cargo waited for native jobs and was stopped with SIGTERM, exit 143. No behavior-level red, green, module typecheck, or successful Engine launch is claimed. Mandatory c2 remains unmet.

Static checks passed: `git diff --check`; `bash etc/scripts/ci/check-crate-deps.sh`. Worker also reported Rust AST parsing and exact metric/dependency contract checks; these are not typechecks.

The real typed R/D launch test is ignored by default and requires a manifest for the exact compiled test executable. Run in a separate Linux process as documented in `etc/systems/ROOT_BENCH.md`. Default package tests do not certify actual Engine execution.

Remaining delivery gaps: fresh genesis only, no pre-F snapshot input, no immutable #6268 baseline executable adapter, transfer-only smoke workload rather than representative performance workload. CPU/RSS/I/O and paired trials belong to base-metal; queue/cache/persistence/cadence accounting remains unsupported. Do not use this draft for performance claims before Linux compilation, runtime correctness, lifecycle, retention, and baseline gates pass.
