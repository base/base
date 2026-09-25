# `base-reth-cli`

Reth-specific CLI utilities for Base execution layer binaries.

## Overview

- **`init_reth!`**: Initializes Reth's global version metadata for P2P identification and logging.
- **Snapshot manifests**: Provides the Base-owned archive generator shared by the execution node's
  `snapshot-manifest` command and the snapshotter sidecar. Existing archives can be reused without
  recompression after verifying their uncompressed BLAKE3 hashes.
- **Framed zstd archives**: Writes and reads `pzstd`-compatible archives made of independent
  zstd frames. The proofs archive uses this layout so Base's downloader can decompress it in
  parallel, while standard zstd decoders still read it sequentially.

## Usage

```toml
[dependencies]
base-reth-cli = { git = "https://github.com/base/base" }
```

```rust,ignore
fn main() {
    base_reth_cli::init_reth!();
    base_reth_cli::init_snapshots!();
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
