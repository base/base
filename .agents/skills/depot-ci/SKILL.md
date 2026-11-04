---
name: depot-ci
description: "Modify Base Depot CI workflows, especially PR cost controls, path filters, runner sizing, or optional integration checks."
---

# Depot CI

Use the workflows in `.depot/workflows/` for Depot CI changes. Keep PR and merge-queue feedback to the cheap, required checks. Expensive suites (System Tests, Base Std Interface Tests, iai and storage benchmarks) run after merge on pushes to `main`, where a failure is fixed after the fact.

- Treat path filters as a risk boundary: include shared workspace, lockfile, toolchain, setup-action, and workflow inputs when they can affect a suite. Prefer running an unnecessary job to omitting relevant coverage.
- Do not make a required check disappear at workflow dispatch time without confirming branch-protection behavior. Prefer a stable workflow with a skipped job, and preserve manual dispatch or a reviewer-label escape hatch for optional work.
- `ci:system`, `ci:base-std`, and `ci:perf` opt a PR into the corresponding expensive suite before merge; `ci:full` opts into all of them. Without a label these suites do not run on PRs or in the merge queue.
- Measure a proposal using runner-size-weighted duration and report the affected suites. Depot billing and wall-clock latency are separate metrics.
