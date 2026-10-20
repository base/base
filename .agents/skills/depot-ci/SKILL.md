---
name: depot-ci
description: "Modify Base Depot CI workflows, especially PR cost controls, path filters, runner sizing, or optional integration checks."
---

# Depot CI

Use the workflows in `.depot/workflows/` for Depot CI changes. Keep normal PR feedback focused on checks relevant to the changed subsystem, while preserving exhaustive validation in `ci-merge-queue.yml`.

- Treat path filters as a risk boundary: include shared workspace, lockfile, toolchain, setup-action, and workflow inputs when they can affect a suite. Prefer running an unnecessary job to omitting relevant coverage.
- Do not make a required check disappear at workflow dispatch time without confirming branch-protection behavior. Prefer a stable workflow with a skipped job, and preserve manual dispatch or a reviewer-label escape hatch for optional work.
- `ci:system`, `ci:base-std`, and `ci:perf` opt a PR into the corresponding expensive suite; `ci:full` opts into all of them. Merge-queue runs are intentionally unconditional.
- Measure a proposal using runner-size-weighted duration and report the affected suites. Depot billing and wall-clock latency are separate metrics.
