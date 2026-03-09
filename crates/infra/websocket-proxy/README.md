# `websocket-proxy`

## Overview

WebSocket proxy that subscribes to new Flashblocks from
[rollup-boost](https://github.com/flashbots/rollup-boost) on the sequencer and broadcasts them
to downstream RPC nodes, minimizing connections to the sequencer and restricting access.
Supports optional Brotli compression for downstream clients.

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
websocket-proxy --upstream-ws ws://sequencer:9000

# Enable Brotli compression for downstream clients
websocket-proxy --upstream-ws ws://sequencer:9000 --enable-compression
```

Run `websocket-proxy --help` for a full list of parameters.

## For Developers

### Building & Testing

You can build and test the project using [Cargo](https://doc.rust-lang.org/cargo/). Some useful commands are:

```
# Build the project
cargo build

# Run all the tests
cargo test --all-features
```

### Deployment

Builds of the websocket proxy are provided.
The only configuration required is the rollup-boost URL to proxy. You can set this via an env var `UPSTREAM_WS` or a flag `--upstream-ws`.

You can see a full list of parameters by running:

`websocket-proxy --help`

### Brotli Compression

The proxy supports compressing messages to downstream clients using Brotli.

To enable this, pass the parameter `--enable-compression`

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
