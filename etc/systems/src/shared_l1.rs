//! Long-lived L1 fixture used to amortize system-test infrastructure startup.

use std::path::Path;

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{L1ContainerConfig, L1Stack, L1StackConfig, SetupContainer};

/// Environment variable pointing at the CI-scoped shared-L1 manifest.
pub const SHARED_L1_RUNTIME_ENV: &str = "BASE_SYSTEM_TEST_SHARED_L1_RUNTIME";

/// Connection details for one CI-scoped L1 fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedL1Runtime {
    /// L1 chain ID.
    pub chain_id: u64,
    /// Docker network on which the L1 containers are reachable.
    pub network_name: String,
    /// Host-reachable L1 JSON-RPC URL.
    pub rpc_url: String,
    /// Container-network L1 JSON-RPC URL.
    pub internal_rpc_url: String,
    /// Host-reachable L1 beacon API URL.
    pub beacon_url: String,
    /// Container-network L1 beacon API URL.
    pub internal_beacon_url: String,
    /// L1 genesis JSON consumed by in-process consensus nodes.
    pub genesis_json: String,
}

impl SharedL1Runtime {
    /// Writes this runtime manifest atomically enough for CI consumers waiting on its existence.
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let file_name = path.file_name().ok_or_else(|| {
            eyre::eyre!("shared L1 runtime path has no file name: {}", path.display())
        })?;
        let temporary = parent.join(format!(".{}.tmp", file_name.to_string_lossy()));
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }

    /// Reads a shared-L1 runtime manifest.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        serde_json::from_slice(&std::fs::read(path)?)
            .wrap_err_with(|| format!("failed to read shared L1 runtime {}", path.display()))
    }

    /// Loads the shared fixture selected for this process, if any.
    pub fn from_env() -> Result<Option<Self>> {
        std::env::var_os(SHARED_L1_RUNTIME_ENV).map_or(Ok(None), |path| Self::load(path).map(Some))
    }
}

/// Owns a long-lived L1 stack and the generated files it requires.
#[derive(Debug)]
pub struct SharedL1 {
    _output_dir: TempDir,
    stack: L1Stack,
    runtime: SharedL1Runtime,
}

impl SharedL1 {
    /// Starts an L1 stack on `network_name` and returns its consumer manifest.
    pub async fn start(network_name: String) -> Result<Self> {
        let output_dir = TempDir::new().wrap_err("failed to create shared L1 output directory")?;
        let setup = SetupContainer::new(output_dir.path());
        let (genesis, _) = tokio::task::spawn_blocking(move || setup.generate_genesis())
            .await
            .wrap_err("shared L1 genesis task panicked")?
            .wrap_err("failed to generate shared L1 genesis")?;
        let genesis_json = genesis.read_el_genesis()?;
        let stack = L1Stack::start(L1StackConfig {
            el_genesis_json: genesis_json.clone(),
            jwt_secret_hex: genesis.read_jwt_secret()?,
            testnet_dir: genesis.testnet_dir(),
            container_config: Some(L1ContainerConfig {
                network_name: Some(network_name.clone()),
                tmpfs_datadir: true,
                ..Default::default()
            }),
        })
        .await
        .wrap_err("failed to start shared L1")?;
        let runtime = SharedL1Runtime {
            chain_id: 1337,
            network_name,
            rpc_url: stack.rpc_url().await?.to_string(),
            internal_rpc_url: stack.reth().internal_rpc_url(),
            beacon_url: stack.beacon_url().await?,
            internal_beacon_url: stack.beacon().internal_beacon_url(),
            genesis_json,
        };
        Ok(Self { _output_dir: output_dir, stack, runtime })
    }

    /// Returns the connection details that consumers use to attach to this L1.
    pub const fn runtime(&self) -> &SharedL1Runtime {
        &self.runtime
    }

    /// Stops the owned L1 containers.
    pub async fn shutdown(self) -> Result<()> {
        drop(self.stack);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SharedL1Runtime;

    #[test]
    fn runtime_manifest_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared-l1.json");
        let runtime = SharedL1Runtime {
            chain_id: 1337,
            network_name: "shared-l1".to_string(),
            rpc_url: "http://127.0.0.1:8545".to_string(),
            internal_rpc_url: "http://l1-reth:8545".to_string(),
            beacon_url: "http://127.0.0.1:4052".to_string(),
            internal_beacon_url: "http://l1-beacon:4052".to_string(),
            genesis_json: "{}".to_string(),
        };
        runtime.write(&path).unwrap();
        assert_eq!(SharedL1Runtime::load(path).unwrap().network_name, "shared-l1");
    }
}
