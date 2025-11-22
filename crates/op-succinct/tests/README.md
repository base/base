# E2E Tests

This directory hosts the end-to-end tests for `op-succinct`, built on top of
Optimism's devstack. Use the `justfile` here to prepare contract artifacts and
run the suite with the expected environment.

## Layout

- `e2e/`: Go e2e tests (currently covers the Succinct validity proposer flow).
- `artifacts/`: Contract artifacts; a compressed tarball lives at
`artifacts/compressed/artifacts.tzst` and is unpacked into `artifacts/src`
before tests.
- `optimism/`: Vendored `succinctlabs/optimism` repository using
`op-succinct-sysgo` branch, which contains `op-succinct` integration code with
Optimism devstack.
- `bindings/`, `presets/`, `utils/`: Presets, helpers and generated code used by
the tests.

## Prerequisites

- Go 1.23+ (matches `go.mod`).
- Rust toolchain (to build the `validity` binary).
- `just`, `zstd`, and `tar` available on your PATH.

## Setup

1) From this directory, fetch/update contract artifacts:

   ```just
   just update-packages
   ```

   This rebuilds artifacts via the vendored Optimism deployer and saves them to
   `artifacts/compressed/artifacts.tzst`.

2) Unpack artifacts (auto-run by the test target, but available standalone):

   ```just
   just unzip-contract-artifacts
   ```

## Running the e2e suite

- Recommended (builds the validity proposer, unpacks artifacts):

  ```just
  just test-e2e-sysgo validity
  ```

- Run a single test with a filter:

  ```just
  just test-e2e-sysgo validity TestValidityProposer_ProveSingleRange
  ```

## Maintenance

- `tests/optimism` is a git submodule that pins the
`succinctlabs/optimism` fork on the `op-succinct-sysgo` branch, which carries
Succinct-specific devstack changes. Rebase that branch onto
`ethereum-optimism/optimism` (`develop` branch) whenever we need upstream fixes,
new OP Stack features, or contract updates that affect the e2e suite.
- Changes to the fork should land via PRs into `succinctlabs/optimism` targeting
`op-succinct-sysgo`.
- After merging fork updates, advance the `tests/optimism` submodule to the new
commit, run `go mod tidy` in `tests` if dependencies changed, regenerate
artifacts with `just update-packages`, and rerun tests to confirm the vendored
stack still passes our tests.
