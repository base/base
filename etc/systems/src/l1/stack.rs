//! L1 stack orchestration (Reth + Lighthouse).

use std::{path::PathBuf, time::Duration};

use alloy_network::Ethereum;
use alloy_provider::{Provider, RootProvider};
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

/// L1 execution layer started before consensus.
///
/// System tests start Reth first so live L2 contract deployment can overlap
/// Lighthouse startup. Deployment talks to the EL RPC; transactions mine once
/// the validator is producing blocks.
#[derive(Debug)]
pub struct L1Execution {
    reth: RethContainer,
    jwt_path: PathBuf,
    testnet_dir: PathBuf,
    container_config: Option<L1ContainerConfig>,
}

impl L1Execution {
    /// Starts Reth and writes the JWT secret if needed.
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

        Ok(Self {
            reth,
            jwt_path,
            testnet_dir: config.testnet_dir,
            container_config: config.container_config,
        })
    }

    /// Returns a reference to the Reth container.
    pub const fn reth(&self) -> &RethContainer {
        &self.reth
    }

    /// Starts Lighthouse beacon and validator on top of the running execution layer.
    pub async fn start_consensus(self) -> Result<L1Stack> {
        let beacon_config = self.container_config.clone();
        let beacon = LighthouseBeaconContainer::start(
            &self.testnet_dir,
            &self.jwt_path,
            self.reth.internal_engine_url(),
            beacon_config,
        )
        .await
        .wrap_err("Failed to start Lighthouse beacon container")?;

        let validator_data_dir = self.testnet_dir.join("validator_data");
        let validator = LighthouseValidatorContainer::start(
            &self.testnet_dir,
            &validator_data_dir,
            beacon.internal_beacon_url(),
            self.container_config,
        )
        .await
        .wrap_err("Failed to start Lighthouse validator container")?;

        Ok(L1Stack { reth: self.reth, beacon, validator, jwt_path: self.jwt_path })
    }
}

#[derive(Debug)]
/// A complete L1 stack comprising Reth and Lighthouse.
pub struct L1Stack {
    reth: RethContainer,
    beacon: LighthouseBeaconContainer,
    validator: LighthouseValidatorContainer,
    #[allow(dead_code)]
    jwt_path: PathBuf,
}

impl L1Stack {
    /// Starts a new L1 stack with the given configuration.
    pub async fn start(config: L1StackConfig) -> Result<Self> {
        L1Execution::start(config).await?.start_consensus().await
    }

    /// Returns a reference to the Reth container.
    pub const fn reth(&self) -> &RethContainer {
        &self.reth
    }

    /// Returns a reference to the Lighthouse beacon container.
    pub const fn beacon(&self) -> &LighthouseBeaconContainer {
        &self.beacon
    }

    /// Maximum time to wait for the L1 validator to propose its first block.
    ///
    /// Sized to cover the remaining beacon genesis lead once the containers are up. Every test
    /// pays this on a stalled L1, so it stays tight enough that a suite-wide L1 failure still
    /// reports per-test results inside the CI budget instead of timing the whole job out.
    pub const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(90);

    /// Poll interval while waiting for the first L1 block.
    const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// Waits until the L1 validator has proposed at least one block after genesis.
    ///
    /// Beacon genesis is stamped when offline genesis generation begins, so the chain only
    /// starts advancing once that timestamp passes. Reth serves RPC the whole time, so callers
    /// that skip this gate see a live endpoint pinned at block 0: L2 derivation never finds an
    /// L1 origin and L1 transactions never leave the pool.
    pub async fn wait_for_block_production(&self) -> Result<()> {
        let provider = RootProvider::<Ethereum>::new_http(self.rpc_url().await?);
        tokio::time::timeout(Self::BLOCK_PRODUCTION_TIMEOUT, async {
            loop {
                if provider.get_block_number().await.is_ok_and(|number| number > 0) {
                    return;
                }
                tokio::time::sleep(Self::BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("L1 did not produce a block before the deadline")
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

    /// Stops the L1 validator and beacon node, leaving the execution layer under test control.
    pub async fn stop_consensus(&self) -> Result<()> {
        self.validator.stop().await.wrap_err("Failed to stop L1 validator")?;
        self.beacon.stop().await.wrap_err("Failed to stop L1 beacon node")?;
        Ok(())
    }
}
