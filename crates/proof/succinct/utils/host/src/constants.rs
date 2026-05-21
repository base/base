use std::{path::PathBuf, sync::LazyLock};

use cargo_metadata::MetadataCommand;

fn get_workspace_root() -> PathBuf {
    let metadata = MetadataCommand::new().exec().unwrap();
    metadata.workspace_root.into()
}

/// Path to the L2 output oracle contract config.
pub static OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var("OP_SUCCINCT_L2_OUTPUT_ORACLE_CONFIG_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| get_workspace_root().join("contracts").join("opsuccinctl2ooconfig.json"))
});

/// Path to the fault dispute game contract config.
pub static OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var("OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| get_workspace_root().join("contracts").join("opsuccinctfdgconfig.json"))
});
