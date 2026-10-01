# Base acceptance CLI

`base-acceptance` is the command-line entry point for the acceptance system in
[`../../acceptance`](../../acceptance/README.md). It supports `validate`, `plan`,
`manifest`, `run`, `check`, `report`, `aggregate`, CI-only `publish`, and recovery
via `cleanup`.

From the repository root:

```console
cargo run -p base-acceptance-cli -- --help
cargo run -p base-acceptance-cli -- validate acceptance/scenarios/smoke.toml
cargo run -p base-acceptance-cli -- run acceptance/scenarios/smoke.toml \
  --output target/acceptance/smoke
```

See the linked guide for the TOML schema, attach mode, report locations, exit
codes, aggregation, recovery, requirements, and current limitations.
