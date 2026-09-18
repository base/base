# `base-snapshotter`

Sidecar for generating and uploading reth node snapshots to S3-compatible storage.

## Overview

Runs alongside a Base execution layer node (base-node-reth) and orchestrates periodic snapshot
creation. `Snapshotter` coordinates the full lifecycle: fetching the EL's latest block and verifying
it is at chain tip (within a configurable freshness window, default 10s), stopping the EL container
via the Docker socket (and the CL container first when `--consensus-container-name` is set),
generating a snapshot manifest and chunk archives for the captured block height using Base's shared
snapshot generator, uploading all artifacts to an S3-compatible store (e.g. Cloudflare R2), then
restarting the EL (followed by the CL when configured, so it reconnects to the EL). Existing
static-file archives are reused only after their compressed size and the BLAKE3 hashes of every
uncompressed source file match the previous manifest.

If the EL is not at tip when a run begins, the snapshot is skipped and containers are left running
untouched.

Proof-history snapshots are opt-in with `--upload-proofs` (or
`SNAPSHOTTER_UPLOAD_PROOFS=1`). When enabled, the sidecar requires a `RocksDB` database at
`{source_datadir}/proofs`. It uploads immutable SST tables once under the shared static-files
prefix and publishes mutable `RocksDB` metadata in each timestamped snapshot run.

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

### RocksDB SST migration compatibility

Snapshots produced with `--upload-proofs` currently publish **two compatible representations**
of each RocksDB database. The normal `proofs` and `rocksdb_indices` manifest components remain
complete archives for the existing `base snapshot download` implementation. The Base-specific
`proofs_static` and `rocksdb_static` manifest extensions contain a small per-run metadata archive
plus content-addressed immutable SST archives for the incremental downloader.

This duplication is intentional and temporary: it lets existing download binaries continue to
restore snapshots throughout the migration. Consumers that understand the extensions restore the
metadata and verify/reuse individual SSTs; consumers that do not ignore the extensions and use
the complete standard components. Do not remove the complete component archives until the
migration gate is explicitly retired.

### Main RocksDB index restore migration

The `rocksdb_static` extension follows the same dual-format migration contract as
`proofs_static`. Reth first restores the complete legacy `rocksdb_indices` component.
The Base downloader then verifies the metadata files and restores only any missing,
verified static SST archives. A manifest without this extension remains a normal legacy
snapshot and the extension step is intentionally a no-op.

#### Controlling legacy archives

`--emit-legacy-rocksdb-archives` (or `SNAPSHOTTER_EMIT_LEGACY_ROCKSDB_ARCHIVES`) defaults
to `true`. It controls whether the complete legacy `proofs.tar.zst` and
`rocksdb_indices.tar.zst` archives are emitted alongside incremental artifacts. Keep it enabled
for the migration deployment. `--emit-legacy-rocksdb-archives=false` is only for controlled
validation after all consumers have an incremental-capable downloader; using it makes newly
published snapshots incompatible with current download binaries.
