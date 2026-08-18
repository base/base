# `base-snapshotter-bin`

Reth snapshot generation and upload sidecar binary for Base.

Parses CLI arguments and delegates to the `base-snapshotter` library, which
orchestrates periodic snapshot creation and upload to S3-compatible storage
alongside a Base execution-layer node.
