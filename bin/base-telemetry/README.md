# `base-telemetry`

Base telemetry backend service.

## Usage

```sh
base-telemetry --listen-addr 0.0.0.0:8080 --p2p-probe-requests-per-minute 2
```

The listen address and P2P probe rate limit can also be configured with
`BASE_TELEMETRY_LISTEN_ADDR` and
`BASE_TELEMETRY_P2P_PROBE_REQUESTS_PER_MINUTE`.

P2P probe requests default to 2 per minute per client IP. The client IP is read
from the `X-Forwarded-For` header only when the direct peer is inside a CIDR
listed in `BASE_TELEMETRY_TRUSTED_PROXY_CIDRS` (default empty); otherwise the
peer socket address is used.

The service exposes health routes and on-demand execution-layer and
consensus-layer reachability checks:

```sh
curl https://telemetry.example/v1/p2p/reachability/el \
  --header 'content-type: application/json' \
  --data '{
    "enode": "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@YOUR_NODE_IP:30303"
  }'
```

The `enode://` URL is printed on node startup and returned by
`admin_nodeInfo`. Replace `YOUR_NODE_IP` with the node's advertised literal
public `IPv4` address. The caller may be the node, an operator, or a monitoring
system; the service probes the IP and TCP port in the supplied enode.

```sh
curl https://telemetry.example/v1/p2p/reachability/cl \
  --header 'content-type: application/json' \
  --data '{
    "enr": "enr:-J64QBw..."
  }'
```

The same endpoint also accepts a public-IPv4 libp2p multiaddr:

```sh
curl https://telemetry.example/v1/p2p/reachability/cl \
  --header 'content-type: application/json' \
  --data '{
    "multiaddr": "/ip4/YOUR_NODE_IP/tcp/9222/p2p/16Uiu2HAm..."
  }'
```

The signed ENR is returned by the consensus node's `opp2p_self` RPC. Either
form must advertise a public literal `IPv4` address, a nonzero TCP port, and a
peer identity — derived from the ENR's secp256k1 key, or taken from the
`/p2p/<peer-id>` component. The service probes that address over TCP + Noise +
Yamux. Both endpoints share the same per-IP rate limit and global probe
capacity.

## Deployment boundary

Results describe reachability from the service's network. Deploy outside the
node's network for an outside-in check, or within the relevant network to check
private nodes. This binary does not provision DNS, TLS certificates, load
balancers, firewall rules, edge rate limiting, or outbound network policy.
