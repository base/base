//! Build script to ensure frontend assets trigger recompilation.

fn main() {
    // Tell Cargo to rerun this build script if any frontend file changes.
    // This is critical for cargo-chef workflows where the crate may be
    // pre-compiled before the frontend assets are copied in.
    println!("cargo::rerun-if-changed=frontend/static");
}
