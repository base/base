use cargo_metadata::MetadataCommand;
use lazy_static::lazy_static;
use std::path::PathBuf;

fn get_workspace_root() -> PathBuf {
    let metadata = MetadataCommand::new().exec().unwrap();
    metadata.workspace_root.into()
}

lazy_static! {
    pub static ref OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH: PathBuf = {
        std::env::var("OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                get_workspace_root().join("contracts").join("opsuccinctl2ooconfig.json")
            })
    };
    pub static ref OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH: PathBuf = {
        std::env::var("OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                get_workspace_root().join("contracts").join("opsuccinctfdgconfig.json")
            })
    };
}
