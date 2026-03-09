# `base-execution-exex`

Execution extensions (`ExEx`) for Base.

## Overview

Implements a live Execution Extension that collects and stores Merkle Patricia Trie proofs
during block execution for use in fault-proof generation. Runs alongside the execution node as a
background task, capturing state proofs incrementally and pruning data outside the proof window.
Supports both batch sync at startup and real-time collection during normal operation.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-exex = { workspace = true }
```

The ExEx is installed into the node builder during node setup and runs automatically as blocks
are processed.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
