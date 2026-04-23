//! Build script for `base-succinct-elfs`.
//!
//! The SP1 ELF binaries are NOT committed to git. This script resolves them by
//! reading `crates/succinct/elf/manifest.toml` and verifying the matching file
//! in the cache directory against the pinned sha256. If a matching file is
//! present, its absolute path is exported as a `cargo:rustc-env=*_ELF_PATH` so
//! that `src/lib.rs` can `include_bytes!(env!(...))` it.
//!
//! If `BASE_SUCCINCT_ELF_STUB=1` is set, empty placeholder files are written
//! instead. This lets `cargo check` / `clippy` / unrelated tests compile
//! without a local SP1 toolchain, at the cost of runtime failure if the
//! constants are actually dereferenced by executed code.

use std::{
    env,
    fs,
    path::{Path, PathBuf},
    process,
};

use sha2::{Digest, Sha256};
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    elfs: Vec<ElfEntry>,
}

#[derive(Deserialize)]
struct ElfEntry {
    name: String,
    sha256: String,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // crate is at crates/succinct/utils/elfs; ELF cache lives at crates/succinct/elf.
    let cache_dir = env::var_os("BASE_SUCCINCT_ELF_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../elf"));
    let manifest_path = cache_dir.join("manifest.toml");

    println!("cargo:rerun-if-env-changed=BASE_SUCCINCT_ELF_STUB");
    println!("cargo:rerun-if-env-changed=BASE_SUCCINCT_ELF_CACHE_DIR");
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let manifest = load_manifest(&manifest_path);
    let stub = env::var("BASE_SUCCINCT_ELF_STUB").as_deref() == Ok("1");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    for entry in &manifest.elfs {
        let env_name = elf_env_var(&entry.name);
        let resolved = if stub {
            write_stub(&out_dir, &entry.name)
        } else {
            resolve_elf(&cache_dir, entry)
        };
        println!("cargo:rerun-if-changed={}", resolved.display());
        println!("cargo:rustc-env={}={}", env_name, resolved.display());
    }
}

fn load_manifest(path: &Path) -> Manifest {
    let contents = fs::read_to_string(path).unwrap_or_else(|err| {
        fail(format!("failed to read ELF manifest at {}: {err}", path.display()))
    });
    toml::from_str(&contents)
        .unwrap_or_else(|err| fail(format!("failed to parse {}: {err}", path.display())))
}

fn resolve_elf(cache_dir: &Path, entry: &ElfEntry) -> PathBuf {
    let path = cache_dir.join(&entry.name);
    let bytes = fs::read(&path).unwrap_or_else(|err| {
        fail(format!(
            "ELF `{name}` not found at {path} ({err}).\n\
             \n\
             Build it with:   just succinct build-elfs\n\
             Or set BASE_SUCCINCT_ELF_STUB=1 for non-proving workflows \
             (check/clippy) - callers will panic at runtime if they \
             dereference the constant.",
            name = entry.name,
            path = path.display(),
        ))
    });
    let actual = hex_sha256(&bytes);
    if actual != entry.sha256 {
        fail(format!(
            "ELF `{name}` sha256 mismatch at {path}.\n\
             expected {expected}\n\
             actual   {actual}\n\
             \n\
             Rebuild with: just succinct build-elfs\n\
             (that target refreshes both the ELF and its hash in manifest.toml.)",
            name = entry.name,
            path = path.display(),
            expected = entry.sha256,
        ));
    }
    path
}

fn write_stub(out_dir: &Path, name: &str) -> PathBuf {
    let stub = out_dir.join(name);
    fs::write(&stub, b"")
        .unwrap_or_else(|err| fail(format!("failed to write stub {}: {err}", stub.display())));
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

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn fail(msg: String) -> ! {
    for line in msg.lines() {
        println!("cargo:warning={line}");
    }
    eprintln!("{msg}");
    process::exit(1);
}
