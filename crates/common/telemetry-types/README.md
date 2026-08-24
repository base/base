# `base-telemetry-types`

Wire schema for Base node telemetry. Serde types only, no I/O.

This crate is the single definition of the version 1 `node_report` payload that
`base-telemetry-client` sends and that the `base-telemetry-service` ingest
endpoint accepts. Both sides depend on this crate so the schema cannot drift.

## Overview

- **`NodeReport`**: the payload a node POSTs to `/v1/ingest`.
- **`ClientMeta`**: version, git sha, network, role, layer.
- **`Heads`**: chain head positions plus the client's own lag samples and
  high-water mark.
- **`Hardware`**: cloud vs bare metal, CPU, memory, and disk.
- **`NodeConfigReport`**: an allowlisted, normalized view of node config.
- **`NetHealth`**: peer counts, churn, and gossip/request error rates.
- **`NodeReportEvent`**: the flattened record the ingest service writes, which
  is a `NodeReport` plus the server-side `received_at`, `reported_ip`, and
  `ip_source`.

## Contract notes

Field names are `snake_case` on the wire because they become log facets
(`hardware.cpu_cores`, `net_health.peer_count`), not because the rest of the repo's HTTP
APIs are. This matches the `base-observability-events` JSONL journal, which is
`snake_case` for the same reason.

Two rules constrain what may be added here:

- **Never the raw command line.** It carries L1 RPC URLs with API keys, JWT
  paths, and signer endpoints. `NodeConfigReport` is an allowlisted, normalized
  set of fields, and `experimental_flags` carries flag *names* only, never
  their values.
- **Never panic messages.** Stack frames are symbols from our own binary and
  are safe; panic messages can carry operator data.

Every field on `NodeReport` is either a scalar the node knows about itself or a
value it already publishes to the p2p network.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
