//! Setup container for L1 genesis and L2 deployment artifacts.

mod container;

pub use container::{
    BUILDER_ENODE_ID, BUILDER_LIBP2P_PEER_ID, BUILDER_P2P_KEY, L1GenesisOutput, L2DeploymentOutput,
    SetupContainer,
};
