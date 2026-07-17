# `base-telemetry-service`

Axum backend for Base telemetry services.

## Routes

- `GET /healthz`
- `GET /readyz`
- `POST /v1/p2p/reachability/el`

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
contain a literal `IPv4` or `IPv6` address; hostnames are not resolved. Private
addresses are allowed, so results describe reachability from the service's
network rather than necessarily from the public internet.

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
  "stage": "rlpx",
  "observedAddress": "YOUR_NODE_IP:30303",
  "elapsedMs": 42,
  "clientVersion": "reth/v1.0.0"
}
```

Invalid requests return `400`, bodies over 1 `KiB` return `413`, and exhausted
probe capacity returns `429`. Probes have a 10-second deadline, with at most 32
running globally. These limits do not apply to the health routes.

## Target selection

The service probes the exact literal `IPv4` or `IPv6` socket address advertised
by the enode, including private and other non-public addresses. Routing,
firewall, and egress policy determine whether the backend can reach the target.
