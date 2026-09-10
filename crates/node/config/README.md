# Node configuration

Base node command-line settings, data directories, version metadata and configuration resolution. Pipeline, storage and networking options live with their execution owners.

Adapted from the local Reth node-core implementation.

`NodeFileConfig` owns persisted TOML settings and backward-compatible loading. It embeds subsystem-owned options instead of redefining them.
