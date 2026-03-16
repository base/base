//! Build script that compiles `proto/zk_prover.proto` into Rust types via
//! tonic/prost. By default only client code is generated; enabling the
//! `server` Cargo feature also generates the server trait and helpers.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build_server = cfg!(feature = "server");
    tonic_prost_build::configure()
        .build_server(build_server)
        .message_attribute(".", "#[allow(missing_docs)]")
        .enum_attribute(".", "#[allow(missing_docs)]")
        .compile_protos(&["proto/zk_prover.proto"], &["proto/"])?;
    Ok(())
}
