# base-action-fixtures

`base-action-fixtures` owns typed, checked-in real-chain fixture data for action
tests. The crate intentionally separates fixture shape, loading, validation,
and capture command plumbing from `base-action-harness`, which remains the
actor-driven test harness.

The expected fixture directory layout is:

```text
fixtures/<network>/<fixture-name>/
├── manifest.toml
├── derivation.json
├── l1.bin.snap
├── l2.json
└── expected.json
```

`manifest.toml` carries provenance and block-range anchors. The JSON files
carry derivation replay anchors, typed L2 blocks, and expected outcomes.
`l1.bin.snap` carries Snappy-compressed bincode for typed L1 blocks.
Normal tests should load fixtures from the catalog or from a fixture directory
and then feed the decoded data through the adapter into the existing
action-harness provider boundaries.

The localized capture binary lives at `actions/fixtures/bin/capture.rs` and is
exposed as `base-action-fixture-capture`. The binary loads an optional local
`.env`, then delegates to the library capture API so meaningful fixture
generation logic stays out of `bin/`. Relative output paths are resolved from
this crate directory, which keeps `just actions fixtures ...` output stable even
though the recipe lives in the `actions` Just module.

By default, generated fixtures are written under:

```text
actions/fixtures/fixtures/{network}/{name}-l2-{l2-start}-{l2-end}/
```

The capture command refuses to replace an existing fixture directory unless
`--overwrite` is passed.

Fixture capture loads `actions/fixtures/.env` when it exists. That file is
ignored by git and should contain local RPC URLs, for example:

```text
BASE_ACTION_FIXTURE_L1_RPC_URL=...
BASE_ACTION_FIXTURE_L2_RPC_URL=...
BASE_ACTION_FIXTURE_NETWORK=base-mainnet
BASE_ACTION_FIXTURE_NAME=base-mainnet-derivation-batch
```

From the workspace root, run capture through the top-level Just modules:

```sh
just actions fixtures --l2-start 4999983 --l2-end 4999983 --overwrite
```

Derivation fixtures include the safe-head anchor, active system config, captured
L2 outputs, and the L1 headers needed to advance to the actual inclusion block
that derives the requested L2 range. The capture path scans L1 in bounded chunks,
probes replay, and trims the fixture at the last L1 block consumed by successful
derivation. Blocks without derivation inputs keep headers only; blocks with
batch inbox, deposit contract, or system config transactions retain only those
transactions and matching receipts.
