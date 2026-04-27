//! Build script that compiles `proto/zk_prover.proto` into Rust types via
//! tonic/prost. By default only client code is generated; enabling the
//! `server` Cargo feature also generates the server trait and helpers.
//! When the `server` feature is active, also emits a file-descriptor set
//! so `tonic-reflection` can serve it at runtime.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build_server = cfg!(feature = "server");
    let mut builder = tonic_prost_build::configure()
        .build_server(build_server)
        .message_attribute(".", "#[allow(missing_docs)]")
        .enum_attribute(".", "#[allow(missing_docs)]");

    if build_server {
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
        builder = builder.file_descriptor_set_path(out_dir.join("prover_descriptor.bin"));
    }

    builder.compile_protos(&["proto/zk_prover.proto"], &["proto/"])?;
    Ok(())
}
