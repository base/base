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
curl --ipv4 https://telemetry.example/v1/p2p/reachability/el \
  --header 'content-type: application/json' \
  --data '{
    "nodeId": "2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4",
    "tcpPort": 30303,
    "addressFamily": "ipv4"
  }'
```

Run this request from the node host or from the same public NAT identity. The
service probes the request's observed public IP and supplied port; it does not
accept an arbitrary target address. Use `--ipv4` with `"ipv4"` or `--ipv6`
with `"ipv6"` so the control connection matches the node's advertised address
family.

## Trusted proxies

Direct or source-preserving deployments need no additional client-IP
configuration. For an HTTP proxy or load balancer, configure every network
that is allowed to supply `X-Forwarded-For`:

```sh
base-telemetry \
  --listen-addr 0.0.0.0:8080 \
  --trusted-proxy-cidr 10.0.0.0/8
```

Multiple CIDRs can also be supplied through a comma-delimited environment
variable:

```sh
BASE_TELEMETRY_TRUSTED_PROXY_CIDRS=10.0.0.0/8,192.168.0.0/16
```

Restrict backend ingress to the configured proxies. Requests carrying
forwarding headers from any other source are rejected.

## Deployment boundary

For a real outside-in check, deploy this service outside the node's network
with a public HTTPS endpoint and outbound TCP access to node P2P ports. This
binary does not provision DNS, TLS certificates, load balancers, firewall
rules, or edge rate limiting.
