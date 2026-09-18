//! Development network outputs and restart validation.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use alloy_primitives::B256;
use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{GenesisConfig, GenesisForge};

/// Generated output locations and their configuration provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisOutput {
    /// Resolved generation inputs, including timestamp and salt.
    pub config: GenesisConfig,
    /// Digest of the verified contract manifest.
    pub contracts: String,
    /// Digest of auxiliary P2P and upgrade-signal settings.
    pub settings: String,
    /// Absolute output paths mapped to their SHA-256 digests.
    pub files: BTreeMap<PathBuf, String>,
}

impl GenesisOutput {
    /// Write a generated file, creating parent directories as needed.
    pub fn write(path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, data)?;
        Ok(())
    }

    /// Write a secret without granting access to other users on Unix.
    pub fn write_secret(path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(path)?.write_all(data)?;
        Ok(())
    }

    /// Read auxiliary devnet settings once so restart fingerprints match the emitted files.
    pub fn settings() -> Result<BTreeMap<String, String>> {
        let mut values: BTreeMap<String, String> =
            serde_json::from_str(include_str!("../assets/keys.json"))?;
        for (key, fallback) in [
            ("UPGRADE_SIGNAL_MODE", "runtime-admin"),
            ("UPGRADE_SIGNAL_L1_BLOCK_TAG", "latest"),
            ("L1_HTTP_PORT", "4545"),
        ] {
            values.insert(key.to_string(), fallback.to_string());
        }
        for (key, value) in &mut values {
            if let Ok(override_value) = std::env::var(key)
                && !override_value.is_empty()
            {
                ensure!(!override_value.contains(['\n', '\r']), "invalid multiline {key}");
                *value = override_value;
            }
        }
        ensure!(
            matches!(
                values["UPGRADE_SIGNAL_MODE"].as_str(),
                "runtime-admin" | "startup-apply" | "metrics-only"
            ),
            "invalid upgrade signal mode"
        );
        ensure!(
            matches!(
                values["UPGRADE_SIGNAL_L1_BLOCK_TAG"].as_str(),
                "latest" | "safe" | "finalized"
            ),
            "invalid upgrade signal block tag"
        );
        ensure!(
            values["L1_HTTP_PORT"].parse::<u16>().is_ok_and(|port| port > 0),
            "invalid L1 HTTP port"
        );
        Ok(values)
    }

    /// Emit public development P2P settings and a fresh random engine JWT.
    pub fn write_keys(output: &Path, settings: &BTreeMap<String, String>) -> Result<()> {
        // Host-side system tests also read these files from a root-owned setup container.
        // Preserve the existing devnet's readable JWT and public development P2P files.
        Self::write(output.join("jwt.hex"), format!("{:x}\n", B256::random()).as_bytes())?;
        for (key, file) in [
            ("BUILDER_P2P_KEY", "builder-p2p-key.txt"),
            ("BUILDER_ENODE_ID", "builder-enode-id.txt"),
            ("L2_EL_BOOTNODE_P2P_KEY", "el-bootnode-p2p-key.txt"),
            ("L2_EL_BOOTNODE_ENODE_ID", "el-bootnode-enode-id.txt"),
            ("L2_EL_BOOTNODE_ENODE", "el-bootnode-enode.txt"),
            ("L2_CL_BOOTNODE_P2P_KEY", "cl-bootnode-p2p-key.txt"),
            ("L2_CL_BOOTNODE_ENR_PATH", "cl-bootnode-enr-path.txt"),
            ("SEQ1_P2P_KEY", "sequencer-1-p2p-key.txt"),
            ("SEQ2_P2P_KEY", "sequencer-2-p2p-key.txt"),
        ] {
            // Reth's P2P key loader parses the whole file without trimming whitespace.
            Self::write(output.join("l2").join(file), settings[key].as_bytes())?;
        }
        Ok(())
    }

    /// Verify that all outputs of a completed generation still exist and are unchanged.
    pub fn validate(
        &self,
        config: &GenesisConfig,
        contracts: &str,
        settings: &BTreeMap<String, String>,
    ) -> Result<()> {
        ensure!(
            self.config == *config
                && self.contracts == contracts
                && self.settings == GenesisForge::digest(&serde_json::to_vec(settings)?),
            "existing genesis has different inputs; choose a new output directory"
        );
        ensure!(!self.files.is_empty(), "empty completion manifest");
        for (path, digest) in &self.files {
            ensure!(
                GenesisForge::digest(&fs::read(path)?) == *digest,
                "completed genesis file changed: {}",
                path.display()
            );
        }
        Ok(())
    }

    /// Publish staged files, preserving their permissions, and record the complete output set.
    pub fn publish(&mut self, staging: &Path, output: &Path) -> Result<()> {
        let stale_signal = output.join("l2/upgrade-signal.env");
        if !self.config.upgrade_signal && stale_signal.exists() {
            fs::remove_file(stale_signal)?;
        }
        let mut pending = vec![staging.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                    continue;
                }
                let path = entry.path();
                let relative = path.strip_prefix(staging)?;
                let destination = output.join(relative);
                fs::create_dir_all(
                    destination.parent().ok_or_else(|| eyre::eyre!("missing output parent"))?,
                )?;
                fs::copy(&path, &destination)?;
                self.files.insert(destination, GenesisForge::digest(&fs::read(path)?));
            }
        }
        Self::write(output.join(".setup-complete"), &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reth_cli_util::get_secret_key;
    use tempfile::tempdir;

    use crate::GenesisOutput;

    #[test]
    fn generated_p2p_keys_load_with_reth() {
        let directory = tempdir().unwrap();
        let settings = serde_json::from_str(include_str!("../assets/keys.json")).unwrap();
        GenesisOutput::write_keys(directory.path(), &settings).unwrap();
        for (name, file) in [
            ("BUILDER_P2P_KEY", "builder-p2p-key.txt"),
            ("L2_EL_BOOTNODE_P2P_KEY", "el-bootnode-p2p-key.txt"),
            ("L2_CL_BOOTNODE_P2P_KEY", "cl-bootnode-p2p-key.txt"),
            ("SEQ1_P2P_KEY", "sequencer-1-p2p-key.txt"),
            ("SEQ2_P2P_KEY", "sequencer-2-p2p-key.txt"),
        ] {
            let key = get_secret_key(&directory.path().join("l2").join(file)).unwrap();
            assert_eq!(key.display_secret().to_string(), settings[name]);
        }
    }
}
