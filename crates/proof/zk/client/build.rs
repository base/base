//! Build script that compiles `proto/zk_prover.proto` into Rust types via
//! tonic/prost. Only client code is generated (`build_server(false)`), and
//! `#[allow(missing_docs)]` is applied to generated messages and enums so
//! the crate-level `#![warn(missing_docs)]` lint does not fire on proto types.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false)
        .message_attribute(".", "#[allow(missing_docs)]")
        .enum_attribute(".", "#[allow(missing_docs)]")
        .compile_protos(&["proto/zk_prover.proto"], &["proto/"])?;
    Ok(())
}
