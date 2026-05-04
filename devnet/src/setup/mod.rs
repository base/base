//! Setup container for L1 genesis and L2 deployment artifacts.

mod container;

pub use container::{
    BUILDER_ENODE_ID, CL_BOOTNODE_ENR_PATH, CL_BOOTNODE_P2P_KEY, EL_BOOTNODE_ENODE,
    EL_BOOTNODE_ENODE_ID, EL_BOOTNODE_P2P_KEY, L1GenesisOutput, L2DeploymentOutput, SetupContainer,
};
