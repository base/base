//! Compiles the prover service protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build_server = cfg!(feature = "server");
    let mut builder = tonic_prost_build::configure()
        .build_server(build_server)
        .type_attribute(".", "#[doc = \"Generated protobuf type.\"]")
        .field_attribute(".", "#[doc = \"Generated protobuf field.\"]");

    if build_server {
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
        builder = builder.file_descriptor_set_path(out_dir.join("prover_service_descriptor.bin"));
    }

    builder.compile_protos(&["proto/prover_service.proto"], &["proto/"])?;
    Ok(())
}
