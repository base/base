# Deferred-root benchmark verification

Authorized branch `bench/deferred-root-engine-replay` started clean at `9bbfcbee3ac12bf40e9ab345671088c0ab57efa1`. Harness changes only; no PoC core edits, commits, pushes, PRs, sessions, toolchain workarounds, fmt or clippy.

## Fail-first evidence

The first six colocated behavior tests were written before production implementation. `cargo test -p base-system-tests --no-default-features --lib root_bench::tests -- --nocapture` was invoked with only those tests and a module declaration present. `red1.log` records the exact dependency failure before implementation began:

```text
libsqlx_macros-a80ed76082b3956f.dylib: dlopen(...): mis-aligned LINKEDIT string pool, fileOffset=0x0066F2E4
sqlx-0.8.6/src/lib.rs:72:1
72 | pub extern crate sqlx_macros;
error: could not compile `sqlx` (lib) due to 1 previous error
```

The initial test binary never compiled or ran. Thus no behavior-level red failure or red-to-green transition is attested. Later CLI/default and timeout/partial-artifact tests and stronger assertions were added during implementation; they have no independent fail-first execution evidence. This mandatory acceptance requirement remains unmet, not waived.

`green-attempt1.log` and `green-attempt2.log` record the same scoped command after implementation and the same unchanged dependency blocker. Each Cargo process was stopped after it reported the SQLx failure while waiting for other native compilation jobs; these processes have SIGTERM termination, not an asserted normal exit code 101. No new test passed or failed at runtime. Final source refinements remain uncompiled. No attempts were made to bypass SQLx or change linker/toolchain settings.

## Checks that passed

- `git diff --check`: `diff-check3.log`, exit 0.
- `bash etc/scripts/ci/check-crate-deps.sh`: `dependency-check2.log`, `All crate dependencies are valid`, exit 0.
- Staging area is empty.
- `Cargo.lock` adds only existing `metrics-exporter-prometheus` and `reth-cli-runner` dependency edges for the systems crate; no versions or pins changed.

## Supported-host work still required

Compile the package, run all eight `root_bench::tests`, and inspect actual admitted nonempty R/D activation results before calling this runnable. Existing node/runner semantic, publication, baseline parity and snapshot regressions were not rerun. Worker panic/drop/timeout injection, delayed-root publication timing, retention stress beyond 128, SIGTERM cleanup, O/S insertion-path observation, and immutable #6268 baseline harness/parity are not verified here. Fresh genesis only; no snapshot option is exposed.

The command is serialized saturation correctness/latency source, not a measured performance result. Throughput includes harness verification/artifact overhead through drain. Base-metal must supply repeated paired trials and OS resource measurements. Current-root metrics prove attempted independent work only when the actual run passes the per-block ready/execution/root checks; source code and VALID alone are not evidence of execution. Actual root values after activation rely on existing core comparison semantics, not an additional harness MPT recomputation.
