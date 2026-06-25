//! Build script for `base-proof-succinct-elfs`.
//!
//! The SP1 ELF binaries are NOT committed to git. This script resolves them
//! from the local ELF cache and exports their absolute paths as
//! `cargo:rustc-env=*_ELF_PATH` so that `src/lib.rs` can `include_bytes!(env!(...))`
//! them.
//!
//! # Environment variables
//!
//! Three env vars control resolution. They operate in three modes:
//!
//! - **Default (neither set):** try to resolve the real ELF from the cache
//!   directory. On any failure, emit a loud `cargo:warning` and fall back to an
//!   empty stub written into `OUT_DIR`. Runtime dereferences of a stub ELF will
//!   panic, but `cargo check` / `rust-analyzer` work on a fresh clone without
//!   requiring the SP1 toolchain.
//! - **`BASE_SUCCINCT_ELF_REQUIRE=1`:** fail the build with a non-zero exit
//!   code instead of falling back to a stub. Use this in release pipelines
//!   and any CI job that must produce real, runnable binaries.
//! - **`BASE_SUCCINCT_ELF_STUB=1`:** skip resolution entirely and always
//!   emit a stub. Useful for docs/lint jobs that never execute the ELFs.
//!   Mutually exclusive with `BASE_SUCCINCT_ELF_REQUIRE=1`; setting both is
//!   a hard error.
//!
//! `BASE_SUCCINCT_ELF_CACHE_DIR` overrides the default cache directory
//! (`crates/proof/succinct/elf`). The script always declares a
//! `cargo:rerun-if-changed` dependency on each expected real ELF path, so
//! populating the cache (e.g. via `just succinct build-elfs`) after a stub-backed
//! build triggers a rebuild.

use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process,
};

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELFS: &[&str] = &["range-elf-embedded", "aggregation-elf"];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // crate is at crates/proof/succinct/utils/elfs; ELF cache lives at crates/proof/succinct/elf.
    let cache_dir = env::var_os("BASE_SUCCINCT_ELF_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../elf"));

    println!("cargo:rerun-if-env-changed=BASE_SUCCINCT_ELF_STUB");
    println!("cargo:rerun-if-env-changed=BASE_SUCCINCT_ELF_REQUIRE");
    println!("cargo:rerun-if-env-changed=BASE_SUCCINCT_ELF_CACHE_DIR");

    let force_stub = env::var("BASE_SUCCINCT_ELF_STUB").as_deref() == Ok("1");
    let require_real = env::var("BASE_SUCCINCT_ELF_REQUIRE").as_deref() == Ok("1");
    if force_stub && require_real {
        fail(
            "BASE_SUCCINCT_ELF_STUB=1 and BASE_SUCCINCT_ELF_REQUIRE=1 are mutually \
             exclusive: the former forces stub ELFs while the latter forbids them. \
             Unset one of the two before retrying.",
        );
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    for name in ELFS {
        let env_name = elf_env_var(name);
        // Always track the expected real ELF path so that populating the
        // cache after a stub-backed build (e.g. `just succinct build-elfs`)
        // invalidates this crate and triggers a rebuild.
        let expected_path = cache_dir.join(name);
        println!("cargo:rerun-if-changed={}", expected_path.display());

        let resolved = if force_stub {
            write_stub(&out_dir, name)
        } else {
            match try_resolve_elf(&cache_dir, name) {
                Ok(path) => path,
                Err(err) => {
                    if require_real {
                        fail(&err);
                    }
                    // Default: warn loudly and fall back to a stub so
                    // `cargo check` / rust-analyzer work without a local SP1 toolchain.
                    warn(&format!(
                        "{err}\n\
                         \n\
                         Falling back to an empty stub ELF. Runtime \
                         dereferences will panic. Run `just succinct \
                         build-elfs` to materialize real ELFs, or set \
                         BASE_SUCCINCT_ELF_REQUIRE=1 to fail fast.",
                    ));
                    write_stub(&out_dir, name)
                }
            }
        };
        println!("cargo:rustc-env={}={}", env_name, resolved.display());
    }
}

fn try_resolve_elf(cache_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let path = cache_dir.join(name);
    let metadata = fs::metadata(&path).map_err(|err| {
        format!("ELF `{name}` not found at {path} ({err}).", path = path.display(),)
    })?;
    if !metadata.is_file() {
        return Err(format!("ELF `{name}` at {path} is not a file.", path = path.display()));
    }
    if metadata.len() == 0 {
        return Err(format!("ELF `{name}` at {path} is empty.", path = path.display()));
    }
    verify_elf_magic(name, &path)?;
    Ok(path)
}

fn verify_elf_magic(name: &str, path: &Path) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|err| {
        format!("failed to open ELF `{name}` at {path}: {err}", path = path.display())
    })?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic).map_err(|err| {
        format!("failed to read ELF magic for `{name}` at {path}: {err}", path = path.display(),)
    })?;
    if &magic != ELF_MAGIC {
        return Err(format!(
            "ELF `{name}` at {path} has invalid magic bytes.",
            path = path.display()
        ));
    }
    Ok(())
}

fn write_stub(out_dir: &Path, name: &str) -> PathBuf {
    let stub = out_dir.join(name);
    fs::write(&stub, b"")
        .unwrap_or_else(|err| fail(&format!("failed to write stub {}: {err}", stub.display())));
    stub
}

fn elf_env_var(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        out.push(match ch {
            'a'..='z' => ch.to_ascii_uppercase(),
            '-' | '.' => '_',
            _ => ch,
        });
    }
    out.push_str("_PATH");
    out
}

fn warn(msg: &str) {
    for line in msg.lines() {
        println!("cargo:warning={line}");
    }
}

fn fail(msg: &str) -> ! {
    warn(msg);
    eprintln!("{msg}");
    process::exit(1);
}
