# Shared CLI support

Process allocator selection, cancellation, command-line value parsers, secret-key loading shared by Base executables.

Adapted from the vendored Reth CLI utilities; this crate has no node or execution dependencies.

Includes the shared Base CLI runner, logging configuration, tracing arguments, metrics endpoint and signal handling. Node-specific command trees live above this crate.
