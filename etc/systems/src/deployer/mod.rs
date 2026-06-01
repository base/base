//! L2 contract deployment via base-deployer.

mod artifacts;
pub use artifacts::DeploymentArtifacts;

mod base_deployer;
pub use base_deployer::{DeployerContainer, RoleAddresses};
