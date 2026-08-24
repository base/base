# `base-telemetry-service`

Axum backend for Base telemetry services.

## Routes

- `GET /healthz`
- `GET /readyz`
- `POST /v1/p2p/reachability/el`
- `POST /v1/p2p/reachability/cl`
- `POST /v1/ingest`

## Node report ingest

`POST /v1/ingest` accepts one `node_report` from a Base node. The payload schema
lives in [`base-telemetry-types`](../../common/telemetry-types) so the client
and this service cannot drift apart.

```http
POST /v1/ingest
Content-Type: application/json

{
  "schema_version": 1,
  "telemetry_id": "01234567-89ab-cdef-0123-456789abcdef",
  "reported_at": "2026-08-21T17:04:11Z",
  "client_version": "1.2.3",
  "git_sha": "abc1234",
  "l2_chain_id": 8453,
  "network": "mainnet",
  "layer": "consensus",
  "role": "validator",
  "uptime_secs": 86400,
  "heads": { "unsafe": 42, "local_safe": 40, "safe": 38, ... },
  "hardware": { "platform": "cloud", "fs_type": "ext4", ... },
  "config": { ... },
  "net_health": { "peer_count": 17, ... }
}
```

The response is `202 Accepted` with an empty body. Recording is asynchronous:
a node must never wait on our storage to finish its reporting cycle. Malformed
bodies return `400` and bodies over 16 `KiB` return `413`. A report declaring an
unrecognized `schema_version` is still recorded with a warning, because a node
must keep reporting across a schema change. The version is an integer major so a
receiver can order it and reject an unknown major; this build warns rather than
rejects, and tightening that is a backend decision, not a client one.

Accepted reports are emitted as a structured log event and, when
`--node-report-path` (or `BASE_TELEMETRY_NODE_REPORT_PATH`) is set, appended as
JSONL through a non-blocking background writer. Each recorded line is the report
plus the three fields only the server can supply: `received_at`, `reported_ip`,
and `ip_source`. `reported_ip` is the node's advertised address when it sent
one and the observed edge IP otherwise.

Ingest has its own rate limit, separate from the probe quota: 60 reports per
hour per client IP by default, configurable with
`--node-report-requests-per-hour` or
`BASE_TELEMETRY_NODE_REPORT_REQUESTS_PER_HOUR`.

## Execution-layer reachability

The reachability endpoint acts as a network observer. A caller sends an
execution-layer `enode://` URL (as printed on node startup and returned by
`admin_nodeInfo`), then the service opens a separate connection to the IP and
TCP port advertised by that enode. A node is reported as `reachable` only after
TCP, ECIES authentication, and the devp2p Hello exchange all complete. A node
that answers the Hello exchange with an authenticated Disconnect (for example
because it is at peer capacity) is still `reachable`; its response omits
`clientVersion`.

The caller may be the node, an operator, or a monitoring system. The enode must
contain a public literal `IPv4` address; hostnames, private addresses, and `IPv6`
addresses are rejected.

Request:

```http
POST /v1/p2p/reachability/el
Content-Type: application/json

{
  "enode": "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@YOUR_NODE_IP:30303"
}
```

Completed probes return HTTP `200` with an outcome of `reachable`,
`connection_failed`, `timed_out`, or `handshake_failed`:

```json
{
  "outcome": "reachable",
  "stage": "devp2p_hello",
  "observedAddress": "YOUR_NODE_IP:30303",
  "elapsedMs": 42,
  "clientVersion": "reth/v1.0.0"
}
```

`stage` identifies where the probe stopped:

- `tcp_connect`: opening the advertised TCP address.
- `encrypted_handshake`: authenticating the encrypted connection with the enode identity.
- `devp2p_hello`: exchanging the Ethereum devp2p Hello message.

Invalid requests return `400`, bodies over 1 `KiB` return `413`, and exhausted
probe capacity returns `429`. Probes have a 10-second deadline. Global probe
concurrency defaults to 32, shared across both layers, configurable with
`--p2p-max-concurrent-probes` or `BASE_TELEMETRY_P2P_MAX_CONCURRENT_PROBES`.
These limits do not apply to the health routes.

## Consensus-layer reachability

The consensus-layer endpoint works the same way for the libp2p network. A
caller sends the node's signed `enr:` record (as returned by `opp2p_self`) or
a public-IPv4 `/ip4/.../tcp/.../p2p/<peer-id>` multiaddr, then the service
opens a separate connection to that public `IPv4` address and TCP port. The
expected libp2p peer identity is derived from the ENR's secp256k1 public key,
or taken from the multiaddr's `/p2p/<peer-id>` component. A node is reported
as `reachable` only after TCP, the Noise handshake against that identity, and
stream multiplexer negotiation all complete. A node that hangs up right after
the connection is established (for example because it is at peer capacity) is
still `reachable`; its response omits `clientVersion`.

Request:

```http
POST /v1/p2p/reachability/cl
Content-Type: application/json

{
  "enr": "enr:-J64QBbwPjPLZ..."
}
```

```http
POST /v1/p2p/reachability/cl
Content-Type: application/json

{
  "multiaddr": "/ip4/YOUR_NODE_IP/tcp/9222/p2p/16Uiu2HAm..."
}
```

Completed probes return HTTP `200` with the same outcome set as the
execution-layer endpoint:

```json
{
  "outcome": "reachable",
  "stage": "identify",
  "observedAddress": "YOUR_NODE_IP:9222",
  "elapsedMs": 42,
  "clientVersion": "op-node/v1.0.0"
}
```

`stage` identifies where the probe stopped:

- `tcp_connect`: opening the advertised TCP address.
- `security_handshake`: authenticating the Noise transport and negotiating the multiplexer.
- `identify`: exchanging libp2p identify information.

Validation, limits, and error responses match the execution-layer endpoint:
the ENR or multiaddr must advertise a public literal `IPv4` address and a
nonzero TCP port. Multiaddrs must be exactly `/ip4/<addr>/tcp/<port>/p2p/<peer-id>`.

## Rate limiting

Reachability requests default to 2 per minute per client IP, configurable with
`--p2p-probe-requests-per-minute` or
`BASE_TELEMETRY_P2P_PROBE_REQUESTS_PER_MINUTE`, and enforced with in-memory GCRA
token buckets. The client IP is taken from the peer socket address unless the
peer is inside a configured set of trusted proxy CIDRs
(`--trusted-proxy-cidrs` or `BASE_TELEMETRY_TRUSTED_PROXY_CIDRS`), in which
case the `X-Forwarded-For` chain is scanned right to left, skipping trusted
proxy hops, and the first untrusted entry is used as the client IP. A missing
or malformed header from a trusted proxy falls back to the peer address with
a warning. Deployments exposed directly (no fronting proxy) leave the
CIDRs empty, so forwarding headers are ignored and the socket peer IP is used.
Limited requests return `429` with a `Retry-After` header and a JSON body of
`{"error":"rate_limited"}`. Limits are tracked per replica; health routes are
never rate limited.

## Target selection

The service probes the exact literal public `IPv4` socket address advertised by
the enode, ENR, or multiaddr. `IPv6` and non-public `IPv4` addresses (loopback,
private, link-local, carrier-grade NAT, multicast, unspecified, and other IANA
special-purpose ranges) are rejected with `400`.
