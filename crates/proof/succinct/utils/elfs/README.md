# Base Proof Succinct ELFs

The zkVM ELF binaries used by the succinct proof pipeline.

The actual binaries live out of tree under `crates/proof/succinct/elf/` and are not committed to git. `build.rs` resolves them against the pinned sha256s in `crates/proof/succinct/elf/manifest.toml` and exposes their absolute paths via `cargo:rustc-env` so the constants in this crate can embed them at compile time.
