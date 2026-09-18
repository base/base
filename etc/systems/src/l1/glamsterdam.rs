//! Explicit, opt-in Amsterdam/Gloas scheduling for the real L1 stack.

use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{L1ContainerConfig, L1GenesisOutput, L1Image};

/// Candidate client provenance. A pin alone is not runtime qualification.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlamsterdamClient {
    /// Exact upstream source revision reported by this image's binary.
    pub source_revision: String,
    /// Immutable, multi-platform registry image reference.
    pub image: String,
}

/// Source-pinned offline generator with the opt-in validator-count patch.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlamsterdamSetup {
    /// Source revision of Base's offline deployment generator.
    pub source_revision: String,
    /// Locally built fixture-only setup image; record its actual image ID on each run.
    pub image: String,
    /// SHA-256 of the checked-in source patch applied during the image build.
    pub patch_sha256: String,
    /// Validators required to populate every minimal-preset slot's PTC candidates.
    pub validator_count: u64,
}

/// Client identities shared by Rust tests and the contributor/CI runner.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlamsterdamClients {
    /// Offline genesis and validator generator.
    pub setup: GlamsterdamSetup,
    /// Ethereum Reth execution client (not Base's L2 execution dependency).
    pub reth: GlamsterdamClient,
    /// Lighthouse beacon and validator client.
    pub lighthouse: GlamsterdamClient,
}

impl GlamsterdamClients {
    /// Reads the checked-in candidate manifest.
    pub fn pinned() -> Result<Self> {
        serde_json::from_str(include_str!("../../fixtures/glamsterdam.json"))
            .wrap_err("invalid Glamsterdam client manifest")
    }

    /// Requires images to have been downloaded before genesis time is resolved.
    pub fn require_local(&self) -> Result<()> {
        for image in [&self.reth.image, &self.lighthouse.image] {
            L1Image::new(image)?;
            let output = Command::new("docker").args(["image", "inspect", image]).output()?;
            ensure!(
                output.status.success(),
                "Glamsterdam image is not available locally; pull before starting the fork clock: {image}"
            );
        }
        let output =
            Command::new("docker").args(["image", "inspect", &self.setup.image]).output()?;
        ensure!(
            output.status.success(),
            "build the fixture setup image before starting the fork clock: {}",
            self.setup.image
        );
        let identity: Value = serde_json::from_slice(&output.stdout)?;
        ensure!(
            identity[0]["Config"]["Labels"]["org.base.devnet.setup.validator-count"] == "1",
            "setup image lacks validator-count support; rebuild it"
        );
        Ok(())
    }

    /// Builds fixture-specific client configuration without changing system-test defaults.
    pub fn container_config(&self, diagnostics_dir: Option<&Path>) -> Result<L1ContainerConfig> {
        Ok(L1ContainerConfig {
            reth_image: Some(L1Image::new(&self.reth.image)?),
            lighthouse_image: Some(L1Image::new(&self.lighthouse.image)?),
            diagnostics_dir: diagnostics_dir.map(Path::to_path_buf),
            network_name: Some(crate::unique_name("glamsterdam-l1")),
            auto_remove_network: true,
            tmpfs_datadir: true,
            ..Default::default()
        })
    }
}

/// A future fork on a minimal-preset testnet with nonempty per-slot PTC committees.
///
/// Defaults use two-second slots and eight-slot epochs, with 384 seconds of pre-fork
/// runway. These deliberately non-production timings do not change Base's L2 rules.
#[derive(Debug, Clone)]
pub struct GlamsterdamConfig {
    /// Nonzero epoch at which Lighthouse activates Gloas.
    pub activation_epoch: u64,
    /// Slot duration shared by generated CL config and the activation calculation.
    pub slot_duration: u64,
}

impl Default for GlamsterdamConfig {
    fn default() -> Self {
        Self { activation_epoch: 24, slot_duration: 2 }
    }
}

/// Resolved schedule retained alongside the rendered EL/CL configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlamsterdamSchedule {
    /// Actual generated genesis timestamp.
    pub genesis_timestamp: u64,
    /// Matched Amsterdam timestamp and Gloas epoch-start timestamp.
    pub activation_timestamp: u64,
    /// Gloas activation epoch.
    pub activation_epoch: u64,
    /// Minimal-preset slots per epoch.
    pub slots_per_epoch: u64,
    /// Seconds per slot.
    pub slot_duration: u64,
}

impl GlamsterdamConfig {
    /// Calculates the activation timestamp, rejecting overflow and genesis activation.
    pub fn activation_timestamp(&self, genesis_timestamp: u64) -> Result<u64> {
        ensure!(
            self.activation_epoch > 0 && self.slot_duration > 0,
            "Glamsterdam must activate after genesis with nonzero slot duration"
        );
        self.activation_epoch
            .checked_mul(8)
            .and_then(|slots| slots.checked_mul(self.slot_duration))
            .and_then(|offset| genesis_timestamp.checked_add(offset))
            .ok_or_else(|| eyre::eyre!("Glamsterdam schedule overflow"))
    }

    /// Adds only inactive future L1 fork configuration to generated Fulu genesis files.
    ///
    /// This does not rewrite genesis time, the genesis state, or any L2 configuration.
    /// Runtime qualification must compare the EL genesis hash to the generated rollup origin.
    pub fn apply(&self, genesis: &L1GenesisOutput) -> Result<GlamsterdamSchedule> {
        let mut el: Value = serde_json::from_str(&genesis.read_el_genesis()?)?;
        let timestamp =
            el["timestamp"].as_str().ok_or_else(|| eyre::eyre!("missing genesis timestamp"))?;
        let genesis_timestamp = u64::from_str_radix(timestamp.trim_start_matches("0x"), 16)?;
        let activation_timestamp = self.activation_timestamp(genesis_timestamp)?;
        ensure!(el["config"]["amsterdamTime"].is_null(), "Amsterdam is already configured");
        ensure!(el["config"]["osakaTime"] == json!(0), "fixture requires pre-fork Osaka genesis");
        let mut cl: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(genesis.cl_config_path())?)?;
        ensure!(
            cl["PRESET_BASE"].as_str() == Some("minimal"),
            "Glamsterdam fixture requires the eight-slot minimal preset"
        );
        ensure!(
            cl["SECONDS_PER_SLOT"].as_u64() == Some(self.slot_duration),
            "EL/CL slot duration mismatch"
        );
        ensure!(
            cl["FULU_FORK_EPOCH"].as_u64() == Some(0),
            "fixture requires pre-fork Fulu genesis"
        );
        ensure!(cl["GLOAS_FORK_EPOCH"].is_null(), "Gloas is already configured");
        cl["GLOAS_FORK_EPOCH"] = serde_yaml::to_value(self.activation_epoch)?;
        cl["GLOAS_FORK_VERSION"] = serde_yaml::Value::String("0x80000000".to_owned());
        el["config"]["amsterdamTime"] = json!(activation_timestamp);
        let blob_parameters = el["config"]["blobSchedule"]["bpo2"].clone();
        ensure!(blob_parameters.is_object(), "missing pre-fork blob parameters");
        el["config"]["blobSchedule"]["amsterdam"] = blob_parameters;
        let schedule = GlamsterdamSchedule {
            genesis_timestamp,
            activation_timestamp,
            activation_epoch: self.activation_epoch,
            slots_per_epoch: 8,
            slot_duration: self.slot_duration,
        };
        schedule.ensure_pre_fork()?;
        std::fs::write(genesis.el_genesis_path(), serde_json::to_vec_pretty(&el)?)?;
        std::fs::write(genesis.cl_config_path(), serde_yaml::to_string(&cl)?)?;
        std::fs::write(
            genesis.testnet_dir().join("glamsterdam-schedule.json"),
            serde_json::to_vec_pretty(&schedule)?,
        )?;
        Ok(schedule)
    }
}

impl GlamsterdamSchedule {
    /// Fails rather than silently turning a missed setup window into a post-fork-only run.
    pub fn ensure_pre_fork(&self) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(
            now < self.activation_timestamp,
            "missed Glamsterdam pre-fork window: now={now}, activation={}",
            self.activation_timestamp
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::GlamsterdamConfig;

    #[test]
    fn matched_timestamp_uses_minimal_epoch_length() {
        assert_eq!(
            GlamsterdamConfig { activation_epoch: 3, slot_duration: 2 }
                .activation_timestamp(100)
                .unwrap(),
            148
        );
    }

    #[test]
    fn rejects_genesis_activation_and_overflow() {
        assert!(
            GlamsterdamConfig { activation_epoch: 0, slot_duration: 2 }
                .activation_timestamp(100)
                .is_err()
        );
        assert!(GlamsterdamConfig::default().activation_timestamp(u64::MAX).is_err());
    }
}
