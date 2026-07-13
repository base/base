# `base-snapshotter`

Sidecar for generating and uploading reth node snapshots to S3-compatible storage.

## Overview

Runs alongside a Base execution layer node (base-node-reth) and orchestrates periodic snapshot
creation. `Snapshotter` coordinates the full lifecycle: verifying the EL is at chain tip (its
latest block is within a configurable freshness window, default 10s), stopping the CL and EL
containers via the Docker socket, generating a snapshot manifest and chunk archives using reth's
`SnapshotManifestCommand`, uploading all artifacts to an S3-compatible store (e.g. Cloudflare R2),
then restarting the EL followed by the CL so it reconnects to the EL.

If the EL is not at tip when a run begins, the snapshot is skipped and both containers are left
running untouched.

The Docker socket (`/var/run/docker.sock`) is volume-mounted into the sidecar container, giving
it control over sibling containers on the host.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-snapshotter = { workspace = true }
```

```rust,ignore
use base_snapshotter::{
    DockerContainerManager, RpcTipChecker, Snapshotter, SnapshotUploader, SnapshotterConfig,
};

let config = SnapshotterConfig::parse();
let container_manager = DockerContainerManager::new(&config.docker_socket)?;
let tip_checker = RpcTipChecker::new(config.el_rpc_url.clone());

// ... create s3_client and uploader ...
let snapshotter = Snapshotter::new(container_manager, tip_checker, uploader, config);
snapshotter.run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
