# Websocket Proxy

## Overview
The Websocket Proxy is a service that subscribes to new Flashblocks from
[rollup-boost](https://github.com/flashbots/rollup-boost) on the sequencer. Then broadcasts them out to any downstream
RPC nodes. Minimizing the number of connections to the sequencer and restricting access.

> **Warning**
>
> This is currently alpha software -- deploy at your own risk!
>
> Currently, this project is a one-directional generic websocket proxy. It doesn't inspect any data or validate clients.
> This may not always be the case.

## For Developers

### Building & Testing
You can build and test the project using [Cargo](https://doc.rust-lang.org/cargo/). Some useful commands are:
```bash
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

### Ring Buffer Replay

The proxy maintains a fixed-size ring buffer of recent flashblocks so that reconnecting
clients can replay the entries they missed without needing to reconnect to the upstream.

**How it works:**

1. As messages arrive from the upstream, they are pushed into the ring buffer (keyed by
   `block_number` and `flashblock_index` extracted from the JSON payload) before being
   broadcast to connected clients.
2. When a client connects, it may include `?block_number=N&flashblock_index=M` query
   parameters in its WebSocket upgrade URL. If present, the proxy replays all buffered
   entries after that position before the client joins the live stream.
3. The subscriber (`WebsocketSubscriber`) tracks the last received position and appends it
   as query parameters when reconnecting to the upstream, enabling upstream-side replay as
   well.

**Configuration:**

```text
--ring-buffer-capacity <N>   Number of flashblocks to retain (default: 55, ~10s replay window)
                             Can also be set via RING_BUFFER_CAPACITY env var.
```

**Client reconnect URL format:**
```text
ws://<host>/ws/?block_number=<N>&flashblock_index=<M>
```

Both `block_number` and `flashblock_index` must be present; if either is absent the replay
is skipped and the client joins the live stream from the current position.
