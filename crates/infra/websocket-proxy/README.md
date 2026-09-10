# `base-infra-websocket-proxy`

## Overview

A generic one-directional WebSocket proxy. It maintains an upstream connection
and broadcasts messages to downstream clients, with optional Brotli compression.
Reconnects use the configured upstream URL without interpreting message payloads.

> **Warning**
>
> This is currently alpha software -- deploy at your own risk!
>
> Currently, this project is a one-directional generic websocket proxy. It doesn't inspect any data or validate clients.
> This may not always be the case.

## Usage

Configure the upstream WebSocket URL via `--upstream-ws` or the `UPSTREAM_WS` environment
variable:

```bash
base-infra-websocket-proxy --upstream-ws ws://sequencer:9000

# Enable Brotli compression for downstream clients
base-infra-websocket-proxy --upstream-ws ws://sequencer:9000 --enable-compression

# Trust client IP forwarding from a proxy network
base-infra-websocket-proxy \
  --upstream-ws ws://sequencer:9000 \
  --trusted-proxy-cidrs 10.0.0.0/8
```

Run `base-infra-websocket-proxy --help` for a full list of parameters.

### Trusted Proxies

Forwarded client IP headers are ignored by default. Configure `--trusted-proxy-cidrs` with every
trusted proxy in the forwarding chain. The direct peer must be trusted before the header selected
by `--ip-addr-http-header` (default: `X-Forwarded-For`) is read.

All forwarding-header lines and comma-separated entries are scanned right to left, skipping
trusted proxies and using the first untrusted IP. Without trusted CIDRs, connection limits use
the direct peer IP.

## For Developers

### Building & Testing

You can build and test the project using [Cargo](https://doc.rust-lang.org/cargo/). Some useful commands are:

```text
# Build the project
cargo build

# Run all the tests
cargo test --all-features
```

### Deployment

Builds of the websocket proxy are provided.
The only configuration required is the upstream WebSocket URL to proxy. You can set this via an env var `UPSTREAM_WS` or a flag `--upstream-ws`.

You can see a full list of parameters by running:

`base-infra-websocket-proxy --help`

### Brotli Compression

The proxy supports compressing messages to downstream clients using Brotli.

To enable this, pass the parameter `--enable-compression`

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
