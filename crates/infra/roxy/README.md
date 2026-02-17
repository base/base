# `base-roxy`

An extensible and modular RPC request router and proxy service built in Rust.

## Overview

This crate provides the components for running the roxy RPC request router
and proxy service. Roxy is a JSON-RPC proxy that sits between clients and
upstream RPC backends. It distributes requests across multiple backends
using a trait-abstracted strategy that defaults to consistent hash routing.

Responses can be cached in a tiered system with an in-memory LRU cache and
optional Redis backing. Rate limiting uses a sliding window algorithm to
control request throughput. The server accepts both HTTP and WebSocket
connections and exposes Prometheus metrics for observability.

## Usage

```rust
//TODO
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
