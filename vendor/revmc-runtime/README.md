# revmc-runtime

Runtime JIT/AOT backend for revmc.

This crate owns the runtime compilation infrastructure: compiled-program lookup, resident-code management, artifact storage, background worker scheduling, AOT preloading, and `revm` integration wrappers.

It builds on `revmc-codegen` for compilation and is re-exported by the umbrella `revmc` crate.
