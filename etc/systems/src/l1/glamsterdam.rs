//! Opt-in Amsterdam/Gloas L1 fixture preparation.

use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    L1ContainerConfig, L1GenesisOutput, L1Image, SetupContainer, SetupImage, SystemTestStackBuilder,
};

const SLOT_DURATION: u64 = 6;
const SLOTS_PER_EPOCH: u64 = 8;
const GLOAS_EPOCH: u64 = 8;
const VALIDATOR_COUNT: u64 = 64;

/// Resolved EL/CL fork schedule for a prepared fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlamsterdamSchedule {
    /// Generated genesis timestamp.
    pub genesis_timestamp: u64,
    /// Amsterdam timestamp and Gloas epoch-start timestamp.
    pub activation_timestamp: u64,
    /// Gloas activation epoch.
    pub activation_epoch: u64,
    /// Slots in the minimal preset epoch.
    pub slots_per_epoch: u64,
    /// Seconds per slot.
    pub slot_duration: u64,
}

/// Prepares a fresh, isolated real-L1 Amsterdam/Gloas fixture.
#[derive(Debug)]
pub struct GlamsterdamFixture;

/// Future Amsterdam/Gloas activation parameters.
#[derive(Debug, Clone)]
pub struct GlamsterdamConfig {
    /// Gloas activation epoch.
    pub activation_epoch: u64,
    /// Consensus slot duration in seconds.
    pub slot_duration: u64,
}

impl Default for GlamsterdamConfig {
    fn default() -> Self {
        Self { activation_epoch: GLOAS_EPOCH, slot_duration: SLOT_DURATION }
    }
}

impl GlamsterdamFixture {
    /// Generates fixture artifacts and returns a stack builder which consumes that prepared L1.
    pub async fn builder(artifacts: &Path) -> Result<SystemTestStackBuilder> {
        // Use the same resolved host path for setup and every subsequent bind mount.
        let artifacts = artifacts.canonicalize().wrap_err("resolve fixture artifact directory")?;
        for artifact in ["el/genesis.json", "cl/genesis.ssz", "cl/config.yaml", "l2/genesis.json"] {
            ensure!(
                !artifacts.join(artifact).exists(),
                "Glamsterdam requires fresh generated state: {} already exists",
                artifacts.join(artifact).display()
            );
        }
        ensure!(
            SetupImage::exists(),
            "devnet-setup:local-v2 must be prebuilt before preparing the fixture"
        );
        let clients: Value = serde_json::from_str(include_str!("../../fixtures/glamsterdam.json"))
            .wrap_err("invalid Glamsterdam client manifest")?;
        let reth = clients["reth"]["image"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("manifest is missing reth.image"))?;
        let lighthouse = clients["lighthouse"]["image"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("manifest is missing lighthouse.image"))?;
        for image in [reth, lighthouse] {
            L1Image::new(image)?;
            let status = Command::new("docker").args(["image", "inspect", image]).status()?;
            ensure!(status.success(), "pinned L1 image is unavailable locally: {image}");
        }

        let output = artifacts.to_path_buf();
        let setup_network = crate::unique_name("glamsterdam-setup");
        let l1_network = crate::unique_name("glamsterdam-l1");
        // Persist ownership before starting any container, including failed startup paths.
        // The command runner can clean these exact networks if the test process is killed.
        std::fs::write(artifacts.join("networks"), format!("{setup_network}\n{l1_network}\n"))?;
        let generated = tokio::task::spawn_blocking(move || {
            SetupContainer::new(&output)
                .with_slot_duration(SLOT_DURATION)
                .with_validator_count(VALIDATOR_COUNT)
                .with_owned_network(setup_network)
                .with_diagnostics_dir(output.join("diagnostics"))
                .generate_genesis()
        })
        .await
        .wrap_err("fixture setup task panicked")??;
        let schedule = GlamsterdamConfig::default().apply(&generated.0)?;
        schedule.ensure_pre_fork()?;
        let container_config = L1ContainerConfig {
            reth_image: Some(L1Image::new(reth)?),
            lighthouse_image: Some(L1Image::new(lighthouse)?),
            diagnostics_dir: Some(artifacts.join("diagnostics")),
            network_name: Some(l1_network),
            auto_remove_network: true,
            tmpfs_datadir: true,
            ..Default::default()
        };
        Ok(SystemTestStackBuilder::new()
            .with_slot_duration(SLOT_DURATION)
            .with_output_dir(artifacts.to_path_buf())
            .with_prepared_l1(generated.0, generated.1, container_config))
    }
}

impl GlamsterdamConfig {
    /// Applies matched future EL and CL activation to fresh generated genesis artifacts.
    pub fn apply(&self, genesis: &L1GenesisOutput) -> Result<GlamsterdamSchedule> {
        ensure!(
            self.activation_epoch > 0 && self.slot_duration > 0,
            "Glamsterdam activation epoch and slot duration must be nonzero"
        );
        let mut el: Value = serde_json::from_str(&genesis.read_el_genesis()?)?;
        let timestamp =
            el["timestamp"].as_str().ok_or_else(|| eyre::eyre!("missing EL genesis timestamp"))?;
        let genesis_timestamp = u64::from_str_radix(timestamp.trim_start_matches("0x"), 16)?;
        let activation_timestamp = self
            .activation_epoch
            .checked_mul(SLOTS_PER_EPOCH)
            .and_then(|slots| slots.checked_mul(self.slot_duration))
            .and_then(|offset| genesis_timestamp.checked_add(offset))
            .ok_or_else(|| eyre::eyre!("Glamsterdam schedule overflow"))?;
        ensure!(el["config"]["amsterdamTime"].is_null(), "Amsterdam is already configured");
        ensure!(el["config"]["osakaTime"] == json!(0), "fixture requires Osaka at genesis");
        let yaml = std::fs::read_to_string(genesis.cl_config_path())?;
        let cl: serde_yaml::Value = serde_yaml::from_str(&yaml)?;
        ensure!(
            cl["PRESET_BASE"].as_str() == Some("minimal"),
            "fixture requires the eight-slot minimal preset"
        );
        ensure!(
            cl["SECONDS_PER_SLOT"].as_u64() == Some(self.slot_duration),
            "generated CL slot duration does not match requested schedule"
        );
        ensure!(cl["FULU_FORK_EPOCH"].as_u64() == Some(0), "fixture requires Fulu at genesis");
        ensure!(cl["GLOAS_FORK_EPOCH"].is_null(), "Gloas is already configured");
        ensure!(cl["GLOAS_FORK_VERSION"].is_null(), "Gloas version is already configured");
        el["config"]["amsterdamTime"] = json!(activation_timestamp);
        let blobs = el["config"]["blobSchedule"]["bpo2"].clone();
        ensure!(blobs.is_object(), "missing pre-fork BPO2 blob parameters");
        el["config"]["blobSchedule"]["amsterdam"] = blobs;
        std::fs::write(genesis.el_genesis_path(), serde_json::to_vec_pretty(&el)?)?;
        // Do not round-trip YAML: serde_yaml would convert existing 0x fork versions to decimal.
        std::fs::write(
            genesis.cl_config_path(),
            format!(
                "{yaml}\nGLOAS_FORK_VERSION: 0x80000000\nGLOAS_FORK_EPOCH: {}\n",
                self.activation_epoch
            ),
        )?;
        let schedule = GlamsterdamSchedule {
            genesis_timestamp,
            activation_timestamp,
            activation_epoch: self.activation_epoch,
            slots_per_epoch: SLOTS_PER_EPOCH,
            slot_duration: self.slot_duration,
        };
        std::fs::write(
            genesis.testnet_dir().join("glamsterdam-schedule.json"),
            serde_json::to_vec_pretty(&schedule)?,
        )?;
        Ok(schedule)
    }
}

impl GlamsterdamSchedule {
    /// Ensures startup can observe a pre-fork interval.
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
    use std::fs;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::{GlamsterdamConfig, GlamsterdamFixture};
    use crate::L1GenesisOutput;

    #[tokio::test]
    async fn rejects_stale_artifacts_before_starting_setup() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("el")).unwrap();
        fs::write(directory.path().join("el/genesis.json"), "stale").unwrap();
        let error = GlamsterdamFixture::builder(directory.path()).await.unwrap_err();
        assert!(error.to_string().contains("requires fresh generated state"));
    }

    fn fixture(el_config: &str, cl_extra: &str) -> (TempDir, L1GenesisOutput, String) {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("el")).unwrap();
        fs::create_dir_all(directory.path().join("cl")).unwrap();
        fs::write(
            directory.path().join("el/genesis.json"),
            format!(r#"{{"timestamp":"0x64","config":{{{el_config}}}}}"#),
        )
        .unwrap();
        let yaml = format!(
            "PRESET_BASE: minimal\nSECONDS_PER_SLOT: 3\nFULU_FORK_VERSION: 0x07000000\nFULU_FORK_EPOCH: 0\n{cl_extra}"
        );
        fs::write(directory.path().join("cl/config.yaml"), &yaml).unwrap();
        let genesis = L1GenesisOutput::from_output_dir(directory.path());
        (directory, genesis, yaml)
    }

    #[test]
    fn applies_explicit_schedule_without_reencoding_existing_yaml() {
        let (_directory, genesis, original_yaml) = fixture(
            r#""osakaTime":0,"amsterdamTime":null,"blobSchedule":{"bpo2":{"target":9}}"#,
            "",
        );
        let schedule =
            GlamsterdamConfig { activation_epoch: 2, slot_duration: 3 }.apply(&genesis).unwrap();
        assert_eq!(schedule.activation_timestamp, 148);
        let el: Value = serde_json::from_str(&genesis.read_el_genesis().unwrap()).unwrap();
        assert_eq!(el["config"]["amsterdamTime"], 148);
        assert_eq!(el["config"]["blobSchedule"]["amsterdam"]["target"], 9);
        assert_eq!(
            fs::read_to_string(genesis.cl_config_path()).unwrap(),
            format!("{original_yaml}\nGLOAS_FORK_VERSION: 0x80000000\nGLOAS_FORK_EPOCH: 2\n")
        );
    }

    #[test]
    fn rejects_invalid_or_preconfigured_schedules() {
        let (_directory, genesis, _) =
            fixture(r#""osakaTime":0,"amsterdamTime":null,"blobSchedule":{"bpo2":{}}"#, "");
        assert!(
            GlamsterdamConfig { activation_epoch: 2, slot_duration: 4 }.apply(&genesis).is_err()
        );

        for (el, cl) in [
            (r#""osakaTime":0,"amsterdamTime":101,"blobSchedule":{"bpo2":{}}"#, ""),
            (
                r#""osakaTime":0,"amsterdamTime":null,"blobSchedule":{"bpo2":{}}"#,
                "GLOAS_FORK_EPOCH: 2\n",
            ),
            (
                r#""osakaTime":0,"amsterdamTime":null,"blobSchedule":{"bpo2":{}}"#,
                "GLOAS_FORK_VERSION: 0x80000000\n",
            ),
        ] {
            let (_directory, genesis, _) = fixture(el, cl);
            assert!(
                GlamsterdamConfig { activation_epoch: 2, slot_duration: 3 }
                    .apply(&genesis)
                    .is_err()
            );
        }
    }
}
