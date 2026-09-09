# `base-common-observability-tracing`

Application logging, reloadable filters, profiling layers, and OpenTelemetry export.

This combines the locally maintained Reth tracing and OTLP support. Log formatting, file and
journald output, subscriber guards, and exporter configuration share one crate. `std` enables
subscriber configuration; `otlp` enables span export, and `otlp-logs` adds log export. Existing
profiling layers remain behind their original feature flags.

With default features disabled, the crate provides the tracing re-export for `no_std` callers.

```sh
cargo test -p base-common-observability-tracing --features otlp-logs
cargo check -p base-common-observability-tracing --no-default-features --target riscv32imac-unknown-none-elf
```
