//! Compiles the vendored libmdbx C sources with the defines reth's `reth-mdbx-sys` uses.
//! The Rust bindings are pre-generated in `src/mdbx_sys.rs` (bindgen 0.72.1 over `libmdbx/mdbx.h`).

use std::{env, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let mdbx = manifest_dir.join("libmdbx");
    println!("cargo:rerun-if-changed={}", mdbx.display());

    let mut cc = cc::Build::new();
    cc.flag_if_supported("-Wno-unused-parameter").flag_if_supported("-Wuninitialized");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() != "linux" {
        cc.flag_if_supported("-Wbad-function-cast");
    }
    let flags = format!("{:?}", cc.get_compiler().cflags_env());
    cc.define("MDBX_BUILD_FLAGS", flags.as_str())
        .define("MDBX_TXN_CHECKOWNER", "0")
        .define("MDBX_USE_FALLOCATE", "0");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux" {
        cc.define("MDBX_USE_MINCORE", "0");
    }
    if env::var("PROFILE").as_deref() == Ok("debug") {
        cc.define("MDBX_DEBUG", "1").define("MDBX_ENABLE_PROFGC", "1");
    } else {
        cc.define("MDBX_DEBUG", "0").define("NDEBUG", None);
    }
    cc.file(mdbx.join("mdbx.c")).compile("libmdbx.a");
}
