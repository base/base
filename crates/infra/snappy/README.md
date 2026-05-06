# `base-snappy`

Sidecar for generating and uploading reth node snapshots to S3-compatible storage.

## Overview

Runs alongside a Base execution layer node (base-node-reth) and orchestrates periodic snapshot
creation. `Snapshotter` coordinates the full lifecycle: stopping the EL container via the Docker
socket, generating a snapshot manifest and chunk archives using reth's `SnapshotManifestCommand`,
uploading all artifacts to an S3-compatible store (e.g. Cloudflare R2), then restarting the EL.

The Docker socket (`/var/run/docker.sock`) is volume-mounted into the sidecar container, giving
it control over sibling containers on the host.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-snappy = { workspace = true }
```

```rust,ignore
use base_snappy::{Snapshotter, SnapshotterConfig};

let config = SnapshotterConfig::parse();
let snapshotter = Snapshotter::from_config(config).await?;
snapshotter.run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
