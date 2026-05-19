# base-proof-succinct-elfs

The zkvm ELF binaries.

The actual binaries live out of tree under `crates/proof/succinct/elf/` and are
not committed to git. `build.rs` resolves them from the local cache and
exposes their absolute paths via `cargo:rustc-env` so the constants below can
embed them at compile time.
