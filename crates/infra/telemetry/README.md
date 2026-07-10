# `base-telemetry-service`

Axum backend for Base telemetry services.

## Routes

- `GET /healthz`
- `GET /readyz`
- `POST /v1/p2p/reachability/el`

## Execution-layer reachability

The reachability endpoint acts as an external observer. A caller sends its
execution-layer `enode://` URL (as printed on node startup and returned by
`admin_nodeInfo`), then the service opens a separate connection to the
caller's observed public IP. A node is reported as `reachable` only after TCP,
ECIES authentication, and the devp2p Hello exchange all complete. A node that
answers the Hello exchange with an authenticated Disconnect (for example
because it is at peer capacity) is still `reachable`; its response omits
`clientVersion`.

The caller must run on the node host or behind the same public NAT. Only the
node identity and TCP port are taken from the enode URL; the IP embedded in it
is never probed, so the API cannot be used to probe arbitrary hosts. Hostnames
in the enode URL are rejected. Callers should connect over the same address
family that the node advertises.

Request:

```http
POST /v1/p2p/reachability/el
Content-Type: application/json

{
  "enode": "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@203.0.113.7:30303"
}
```

Completed probes return HTTP `200` with an outcome of `reachable`,
`connection_failed`, `timed_out`, or `handshake_failed`:

```json
{
  "outcome": "reachable",
  "stage": "rlpx",
  "observedAddress": "8.8.8.8:30303",
  "elapsedMs": 42,
  "clientVersion": "reth/v1.0.0"
}
```

Invalid requests or source headers return `400`, bodies over 1 `KiB` return
`413`, and exhausted probe capacity returns `429`. Probes have a 10-second
deadline, with at most 32 running globally and one per source IP. These limits
do not apply to the health routes.

## Client IP policy

Direct requests use the TCP peer address. `X-Forwarded-For` is accepted only
when the socket peer belongs to a configured trusted proxy CIDR. The service
walks the complete chain from right to left, skips trusted proxy hops, and uses
the first untrusted globally routable address. Missing, malformed, spoofed, or
all-trusted chains are rejected without falling back to the proxy address.

Configure trusted networks only when the service is behind a proxy that
maintains this forwarding chain:

```sh
base-telemetry \
  --trusted-proxy-cidr 10.0.0.0/8 \
  --trusted-proxy-cidr 192.168.0.0/16
```

The equivalent environment variable is
`BASE_TELEMETRY_TRUSTED_PROXY_CIDRS=10.0.0.0/8,192.168.0.0/16`.
