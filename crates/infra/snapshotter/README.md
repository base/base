# `base-snapshotter`

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
base-snapshotter = { workspace = true }
```

```rust,ignore
use base_snapshotter::{DockerContainerManager, Snapshotter, SnapshotUploader, SnapshotterConfig};

let config = SnapshotterConfig::parse();
let container_manager = DockerContainerManager::new(&config.docker_socket)?;

// ... create s3_client and uploader ...
let snapshotter = Snapshotter::new(container_manager, uploader, config);
snapshotter.run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
