# Snapshot archives

`base-execution-state-maintenance` owns snapshot schemas, archive creation, and BLAKE3
checksums shared by the node CLI and snapshotter service. The generator reuses existing
archives when their uncompressed source files match the previous manifest.

Callers provide the producer version recorded in the manifest. Command-line parsing and
node startup remain in the node layer.
