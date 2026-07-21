# `base-telemetry`

Base telemetry backend service.

## Usage

```sh
base-telemetry --listen-addr 0.0.0.0:8080
```

The listen address can also be configured with `BASE_TELEMETRY_LISTEN_ADDR`.

The service exposes health routes and an on-demand execution-layer
reachability check:

```sh
curl https://telemetry.example/v1/p2p/reachability/el \
  --header 'content-type: application/json' \
  --data '{
    "enode": "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@YOUR_NODE_IP:30303"
  }'
```

The `enode://` URL is printed on node startup and returned by
`admin_nodeInfo`. Replace `YOUR_NODE_IP` with the node's advertised literal
`IPv4` or bracketed `IPv6` address. The caller may be the node, an operator, or a
monitoring system; the service probes the IP and TCP port in the supplied
enode, including private addresses reachable from the service's network.

## Deployment boundary

Results describe reachability from the service's network. Deploy outside the
node's network for an outside-in check, or within the relevant network to check
private nodes. This binary does not provision DNS, TLS certificates, load
balancers, firewall rules, edge rate limiting, or outbound network policy.
