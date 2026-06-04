//! L1 stack orchestration (Reth + Lighthouse).

use std::path::PathBuf;

use eyre::{Result, WrapErr};
use url::Url;

use super::{
    L1ContainerConfig, LighthouseBeaconContainer, LighthouseValidatorContainer, RethContainer,
};

/// Configuration for the L1 stack.
#[derive(Debug, Default)]
pub struct L1StackConfig {
    /// JSON content for the EL genesis.
    pub el_genesis_json: String,
    /// Hex-encoded JWT secret.
    pub jwt_secret_hex: String,
    /// Path to the testnet directory.
    pub testnet_dir: PathBuf,
    /// Optional stable container configuration.
    pub container_config: Option<L1ContainerConfig>,
}

#[derive(Debug)]
/// A complete L1 stack comprising Reth and Lighthouse.
pub struct L1Stack {
    reth: RethContainer,
    beacon: LighthouseBeaconContainer,
    #[allow(dead_code)]
    validator: LighthouseValidatorContainer,
    #[allow(dead_code)]
    jwt_path: PathBuf,
}

impl L1Stack {
    /// Starts a new L1 stack with the given configuration.
    pub async fn start(config: L1StackConfig) -> Result<Self> {
        let jwt_path = config.testnet_dir.parent().unwrap_or(&config.testnet_dir).join("jwt.hex");

        if !jwt_path.exists() {
            std::fs::write(&jwt_path, &config.jwt_secret_hex)
                .wrap_err("Failed to write JWT secret")?;
        }

        let reth_config = config.container_config.clone();
        let reth =
            RethContainer::start(&config.el_genesis_json, &config.jwt_secret_hex, reth_config)
                .await
                .wrap_err("Failed to start Reth container")?;

        let beacon_config = config.container_config.clone();
        let beacon = LighthouseBeaconContainer::start(
            &config.testnet_dir,
            &jwt_path,
            reth.internal_engine_url(),
            beacon_config,
        )
        .await
        .wrap_err("Failed to start Lighthouse beacon container")?;

        let validator_data_dir = config.testnet_dir.join("validator_data");
        let validator_config = config.container_config.clone();
        let validator = LighthouseValidatorContainer::start(
            &config.testnet_dir,
            &validator_data_dir,
            beacon.internal_beacon_url(),
            validator_config,
        )
        .await
        .wrap_err("Failed to start Lighthouse validator container")?;

        Ok(Self { reth, beacon, validator, jwt_path })
    }

    /// Returns a reference to the Reth container.
    pub const fn reth(&self) -> &RethContainer {
        &self.reth
    }

    /// Returns a reference to the Lighthouse beacon container.
    pub const fn beacon(&self) -> &LighthouseBeaconContainer {
        &self.beacon
    }

    /// Returns the public RPC URL of the Reth container.
    pub async fn rpc_url(&self) -> Result<Url> {
        self.reth.rpc_url().await
    }

    /// Returns the public Engine API URL of the Reth container.
    pub async fn engine_url(&self) -> Result<Url> {
        self.reth.engine_url().await
    }

    /// Returns the public URL of the Lighthouse beacon container.
    pub async fn beacon_url(&self) -> Result<String> {
        self.beacon.beacon_url().await
    }
}
