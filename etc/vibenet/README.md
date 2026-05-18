# vibenet

A public, ephemeral devnet for showing off in-flight Base features.

- Single L1 (anvil) + single L2 sequencer (same as `just up-single`)
- Optional public TLS gateway (Caddy) for the hosted environment
- Per-method JSON-RPC rate limiting at proxyd
- Open RPC (no API key)
- One prefunded faucet address; standard anvil EOAs are swept into it
- Test contracts (`USDV` — public-mint ERC-20, `NFV` — public-mint
  ERC-721) auto-deployed on boot
- Landing page + faucet UI + block explorer, all served by the Next.js
  app from the [`base/ui`](https://github.com/base/ui) repo

## Public hostnames

| Hostname                   | What it serves              |
| -------------------------- | --------------------------- |
| `vibes.base.org`           | Landing page                |
| `rpc.vibes.base.org`       | JSON-RPC + WebSocket        |
| `explorer.vibes.base.org`  | Block explorer              |
| `faucet.vibes.base.org`    | Faucet UI + drip API        |

All UI subdomains route to the same Next.js app; the app reads the
`Host` header and rewrites internally to the right page.

## Quick links

- Host env template: [`vibenet-env.example`](./vibenet-env.example)
- UI content (editable per branch): [`config/vibenet.yaml`](./config/vibenet.yaml)
- Contract list (editable per branch): [`setup/contracts.yaml`](./setup/contracts.yaml)
- Caddyfile (production gateway): [`caddy/Caddyfile`](./caddy/Caddyfile)

## Running locally

Vibenet can run locally or on a hosted environment. Local runs skip the
`:443` Caddy overlay entirely — `just vibe` only enables it on hosts
that have TLS files installed under `/etc/vibenet/tls`.

```bash
# One-time: clone base/ui as a sibling of this repo
git clone git@github.com:base/ui.git ../ui

# One-time: copy the example env and fill in values. FAUCET_PRIVATE_KEY /
# FAUCET_ADDR are required; the public listener knobs are only used in
# production.
cp etc/vibenet/vibenet-env.example etc/vibenet/vibenet-env
${EDITOR} etc/vibenet/vibenet-env

just vibe
```

The Next.js container publishes loopback-only host ports:

| URL                                 | Service                                       |
| ----------------------------------- | --------------------------------------------- |
| `http://localhost:18080/`           | Landing page                                  |
| `http://localhost:18080/faucet`     | Faucet UI + API                               |
| `http://localhost:18080/explorer`   | Block explorer                                |
| `http://localhost:18080/api/...`    | Faucet + explorer + config/contracts APIs     |
| `ws://localhost:18081/`             | WebSocket RPC (base-client direct)            |
| `http://localhost:18082/`           | HTTP RPC (proxyd)                             |

Override the bindings with `VIBENET_HOST_PORT` / `VIBENET_WS_HOST_PORT` /
`VIBENET_RPC_HOST_PORT` in `vibenet-env` if those collide with something
else on your machine.

Quick smoke test once `just vibe` is up:

```bash
curl -s http://localhost:18080/api/vibenet/config | jq .title

curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
  http://localhost:18082
```

To skip cloning `base/ui` and use a pre-built image instead:

```bash
VIBENET_UI_IMAGE=ghcr.io/base/ui:main just vibe
```

## Iterating on the UI

Use `just vibe-ui` to rebuild just the Next.js container without resetting
the chain. Block history, deployed contracts, and the explorer index all
survive. The vibenet-config-renderer is also restarted so changes to
`config/vibenet.yaml` take effect.

## Customizing what appears on the landing page

Edit [`config/vibenet.yaml`](./config/vibenet.yaml). The
`vibenet-config-renderer` container reads it at startup, converts to
JSON, and writes it to a shared volume that the Next.js app serves at
`/api/vibenet/config`. No image rebuild required.

Fields:

- `title`, `subtitle` — page header
- `features` — array of `{title, description, link?}` cards
- `branch`, `commit` — auto-overwritten by `just vibe` from `git rev-parse`

## Customizing deployed contracts

Edit [`setup/contracts.yaml`](./setup/contracts.yaml) and drop any new
Solidity sources into [`setup/contracts/src/`](./setup/contracts/). Each
entry is:

```yaml
- name: myDemo                              # key in contracts.json
  artifact: src/MyDemo.sol:MyDemo           # forge target
  args: ["0x1234...", "{{ usdv }}"]         # optional; {{ }} resolves from
                                            # previously-deployed entries
```

Deployed addresses are published at `/api/vibenet/contracts` and surfaced
on the landing page automatically.

## Faucet integration

The faucet's hot wallet (`FAUCET_ADDR`) is prefunded on L1 via the
genesis injection in `setup-l1.sh`, then topped up on L2 by `vibenet-setup`
sweeping the standard anvil EOAs into it at boot.

Drips supported:
- ETH (`POST /api/vibenet/faucet/drip`)
- USDV mint (`POST /api/vibenet/faucet/drip-usdv`) — calls `mint(addr, amount)`
- NFV mint (`POST /api/vibenet/faucet/drip-nfv`) — calls `mint(addr)`

## RPC access

The RPC is currently open; no API key is required.

```bash
curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
  https://rpc.vibes.base.org
```

WebSocket:

```javascript
new WebSocket("wss://rpc.vibes.base.org/ws");
```

Rate limits are configured in proxyd.

## Components

| Container                 | Image                                | Role |
| ------------------------- | ------------------------------------ | ---- |
| `next-app`                | `vibenet-ui:local` (built from `../ui`) | Landing page, faucet UI+API, block explorer (Next.js + better-sqlite3) |
| `vibenet-setup`           | `vibenet-setup:local` (foundry)      | One-shot: waits for L2, sweeps anvil balances, deploys demo contracts |
| `vibenet-config-renderer` | `mikefarah/yq`                       | Converts `vibenet.yaml` to `config.json` |
| `proxyd`                  | `proxyd:local`                       | Per-method JSON-RPC rate limits |
| `caddy` (prod only)       | `caddy:2.9-alpine`                   | TLS termination + subdomain routing |
| `base-client/builder/...` | same as `just up-single`             | Core devnet |

## File map

```
etc/vibenet/
  README.md                            (this file)
  vibenet-env.example                  host env template
  docker-compose.vibenet.yml           overlay on etc/docker/docker-compose.yml
  docker-compose.caddy.yml             prod-only overlay: Caddy TLS gateway
  config/vibenet.yaml                  editable UI content
  caddy/Caddyfile                      production TLS + subdomain routing
  proxyd/proxyd-ratelimit.toml         per-method rate limits
  setup/Dockerfile                     build image for foundry-based deployer
  setup/contracts.yaml                 list of contracts to deploy
  setup/contracts/                     foundry project: src/*.sol
  setup/deploy-contracts.sh            entrypoint for vibenet-setup

# UI source lives in a separate repo:
../ui/                                 base/ui (Next.js app)
```
