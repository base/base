# `websocket-proxy`

## Overview

WebSocket proxy that relays the sequencer builder's block stream to downstream clients,
minimizing connections to the sequencer and restricting access. Supports optional Brotli
compression for downstream clients.

The proxy is payload-agnostic: it forwards each upstream text or binary frame unchanged, without
parsing, filtering, replaying, or deduplicating it. Downstream clients receive every message as
a binary frame.

After Denim the builder publishes each sealed block in the P2P block encoding: a snappy-compressed
SSZ `BaseExecutionPayloadEnvelope` (32-byte parent beacon block root, then the SSZ execution
payload). These are the bytes the sequencer signs for P2P gossip, without the signature. Clients
decode, validate, and execute each block, and derive receipts and logs locally.

The stream is best effort and live-only. Blocks are published when the sequencer seals them,
before they become canonical, so clients may see gaps, duplicates, or reorged blocks. A
reconnect starts at the live tail. Recover missed or historical blocks over P2P or RPC.

## Usage

Configure the upstream WebSocket URL via `--upstream-ws` or the `UPSTREAM_WS` environment
variable:

```bash
websocket-proxy --upstream-ws ws://sequencer:9000

# Enable Brotli compression for downstream clients
websocket-proxy --upstream-ws ws://sequencer:9000 --enable-compression

# Trust client IP forwarding from a proxy network
websocket-proxy \
  --upstream-ws ws://sequencer:9000 \
  --trusted-proxy-cidrs 10.0.0.0/8
```

Run `websocket-proxy --help` for a full list of parameters.

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
The only configuration required is the builder block stream URL to proxy. You can set this via an env var `UPSTREAM_WS` or a flag `--upstream-ws`.

You can see a full list of parameters by running:

`websocket-proxy --help`

### Brotli Compression

The proxy supports compressing messages to downstream clients using Brotli.

To enable this, pass the parameter `--enable-compression`

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
