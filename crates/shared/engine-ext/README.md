# base-engine-ext

In-process engine client for direct reth communication.

## Overview

This crate provides `InProcessEngineClient`, which bypasses the RPC layer
to communicate directly with reth's consensus engine via channels.

## Architecture

```text
┌─────────────────┐     ┌──────────────────────────┐     ┌─────────────────────────┐
│  Consensus      │     │ InProcessEngineClient    │     │  reth Engine            │
│  (kona-node)    │────▶│ (channel-based)          │────▶│  (EngineApiTreeHandler) │
└─────────────────┘     └──────────────────────────┘     └─────────────────────────┘
```

This enables latency reduction from ~1-5ms (HTTP) to ~1μs (channel).
