//! Build script that compiles `proto/zk_prover.proto` into Rust client
//! types via tonic/prost.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false)
        .message_attribute(".", "#[allow(missing_docs)]")
        .enum_attribute(".", "#[allow(missing_docs)]")
        .compile_protos(&["proto/zk_prover.proto"], &["proto/"])?;
    Ok(())
}
