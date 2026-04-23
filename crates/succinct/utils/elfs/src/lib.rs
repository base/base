//! The zkvm ELF binaries.
//!
//! The actual binaries live out of tree under `crates/succinct/elf/` and are
//! not committed to git. `build.rs` resolves them against the pinned sha256s
//! in `crates/succinct/elf/manifest.toml` and exposes their absolute paths via
//! `cargo:rustc-env` so the constants below can embed them at compile time.

/// Aggregation program ELF binary.
pub const AGGREGATION_ELF: &[u8] = include_bytes!(env!("AGGREGATION_ELF_PATH"));

/// Range program ELF binary.
pub const RANGE_ELF_EMBEDDED: &[u8] = include_bytes!(env!("RANGE_ELF_EMBEDDED_PATH"));
