//! Configuration loading and resolution for `base-deployer`.

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use alloy_primitives::U256;
use eyre::{Result, WrapErr, bail};
use rand::Rng as _;
use serde::{Deserialize, Serialize};

const DEFAULT_OUTPUT_DIR: &str = "devnet";
const DEFAULT_SLOT_DURATION: u64 = 4;
const DEFAULT_PREFUND_BALANCE: &str = "0xd3c21bcecceda1000000";
const L1_CHAIN_ID_MIN: u64 = 1_300_000;
const L1_CHAIN_ID_MAX: u64 = 1_399_999;
const L2_CHAIN_ID_MIN: u64 = 84_530_000;
const L2_CHAIN_ID_MAX: u64 = 84_539_999;

/// File-backed configuration for `base-deployer`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct DeployerConfig {
    /// Output directory for generated artifacts.
    pub(crate) output_dir: Option<PathBuf>,
    /// L1 chain ID for the generated devnet.
    pub(crate) l1_chain_id: Option<u64>,
    /// L2 chain ID for the generated devnet.
    pub(crate) l2_chain_id: Option<u64>,
    /// L1 beacon slot duration in seconds.
    pub(crate) slot_duration: Option<u64>,
    /// Unix timestamp for genesis.
    pub(crate) genesis_time: Option<u64>,
    /// Prefund balance for dev accounts, in hex or decimal wei.
    pub(crate) prefund_balance: Option<String>,
    /// Optional block number used to derive Base V1 activation time.
    pub(crate) l2_base_v1_block: Option<u64>,
}

/// Resolved runtime configuration after applying defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedConfig {
    /// Output directory for artifacts.
    pub(crate) output_dir: PathBuf,
    /// L1 chain ID.
    pub(crate) l1_chain_id: u64,
    /// L2 chain ID.
    pub(crate) l2_chain_id: u64,
    /// L1 beacon slot duration in seconds.
    pub(crate) slot_duration: u64,
    /// Unix timestamp for genesis.
    pub(crate) genesis_time: u64,
    /// Prefund balance for dev accounts.
    pub(crate) prefund_balance: U256,
    /// Optional block number used to derive Base V1 activation time.
    pub(crate) l2_base_v1_block: Option<u64>,
}

/// Serialized chain ID metadata written alongside generated artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainIds {
    /// L1 chain ID.
    pub(crate) l1_chain_id: u64,
    /// L2 chain ID.
    pub(crate) l2_chain_id: u64,
}

impl DeployerConfig {
    /// Loads a configuration file from JSON or TOML.
    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read config file at {}", path.display()))?;

        match path.extension().and_then(std::ffi::OsStr::to_str) {
            Some("json") => serde_json::from_str(&contents)
                .wrap_err_with(|| format!("Failed to parse JSON config at {}", path.display())),
            Some("toml") => toml::from_str(&contents)
                .wrap_err_with(|| format!("Failed to parse TOML config at {}", path.display())),
            _ => serde_json::from_str(&contents)
                .or_else(|_| toml::from_str(&contents))
                .wrap_err_with(|| {
                    format!(
                        "Failed to parse config at {} as JSON or TOML",
                        path.display()
                    )
                }),
        }
    }

    /// Resolves defaults, random chain IDs, and output directory safety checks.
    pub(crate) fn resolve(self, output_dir_override: Option<PathBuf>) -> Result<ResolvedConfig> {
        let output_dir = self.output_dir(output_dir_override)?;
        let chain_ids = self.resolve_chain_ids(&output_dir, None)?;
        self.finish_resolution(output_dir, chain_ids)
    }

    /// Resolves defaults while pinning the L1 chain ID to a detected live value.
    pub(crate) fn resolve_with_l1_chain_id(
        self,
        output_dir_override: Option<PathBuf>,
        detected_l1_chain_id: u64,
    ) -> Result<ResolvedConfig> {
        let output_dir = self.output_dir(output_dir_override)?;
        let chain_ids = self.resolve_chain_ids(&output_dir, Some(detected_l1_chain_id))?;
        self.finish_resolution(output_dir, chain_ids)
    }
}

impl ResolvedConfig {
    /// Returns the chain ID bundle.
    pub(crate) const fn chain_ids(&self) -> ChainIds {
        ChainIds { l1_chain_id: self.l1_chain_id, l2_chain_id: self.l2_chain_id }
    }
}

impl DeployerConfig {
    fn output_dir(&self, output_dir_override: Option<PathBuf>) -> Result<PathBuf> {
        let output_dir = output_dir_override
            .or_else(|| self.output_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIR));
        ensure_output_dir_is_safe(&output_dir)?;
        Ok(output_dir)
    }

    fn resolve_chain_ids(
        &self,
        output_dir: &Path,
        detected_l1_chain_id: Option<u64>,
    ) -> Result<ChainIds> {
        let existing = load_existing_chain_ids(output_dir)?;
        let explicit_l1 = self.l1_chain_id.or(existing.map(|ids| ids.l1_chain_id));
        let explicit_l2 = self.l2_chain_id.or(existing.map(|ids| ids.l2_chain_id));

        if let (Some(expected), Some(detected)) = (explicit_l1, detected_l1_chain_id)
            && expected != detected
        {
            bail!(
                "Configured L1 chain ID {} does not match detected live L1 chain ID {}",
                expected,
                detected
            );
        }

        Ok(resolve_chain_ids(explicit_l1.or(detected_l1_chain_id), explicit_l2))
    }

    fn finish_resolution(self, output_dir: PathBuf, chain_ids: ChainIds) -> Result<ResolvedConfig> {
        Ok(ResolvedConfig {
            output_dir,
            l1_chain_id: chain_ids.l1_chain_id,
            l2_chain_id: chain_ids.l2_chain_id,
            slot_duration: self.slot_duration.unwrap_or(DEFAULT_SLOT_DURATION),
            genesis_time: self.genesis_time.unwrap_or_else(current_unix_timestamp),
            prefund_balance: parse_balance(self.prefund_balance.as_deref())?,
            l2_base_v1_block: self.l2_base_v1_block,
        })
    }
}

fn resolve_chain_ids(l1_chain_id: Option<u64>, l2_chain_id: Option<u64>) -> ChainIds {
    let mut rng = rand::rng();
    let l1 = l1_chain_id.unwrap_or_else(|| rng.random_range(L1_CHAIN_ID_MIN..=L1_CHAIN_ID_MAX));

    let l2 = l2_chain_id.unwrap_or_else(|| {
        loop {
            let candidate = rng.random_range(L2_CHAIN_ID_MIN..=L2_CHAIN_ID_MAX);
            if candidate != l1 {
                break candidate;
            }
        }
    });

    ChainIds { l1_chain_id: l1, l2_chain_id: l2 }
}

/// Loads chain IDs from an existing artifact bundle, if present.
pub(crate) fn load_existing_chain_ids(output_dir: &Path) -> Result<Option<ChainIds>> {
    let path = output_dir.join("chain-ids.json");
    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("Failed to read existing chain IDs at {}", path.display()))?;
    serde_json::from_str(&contents)
        .map(Some)
        .wrap_err_with(|| format!("Failed to parse existing chain IDs at {}", path.display()))
}

fn ensure_output_dir_is_safe(output_dir: &Path) -> Result<()> {
    if !output_dir.exists() {
        return Ok(());
    }

    let contains_source_tree =
        output_dir.join("Cargo.toml").exists() || output_dir.join("src").exists();
    if contains_source_tree {
        bail!(
            "Refusing to write devnet artifacts into {} because it looks like a source directory. \
Pass --output-dir to an empty or dedicated artifacts path.",
            output_dir.display()
        );
    }

    Ok(())
}

fn parse_balance(balance: Option<&str>) -> Result<U256> {
    let value = balance.unwrap_or(DEFAULT_PREFUND_BALANCE);

    if let Some(hex) = value.strip_prefix("0x") {
        return U256::from_str_radix(hex, 16)
            .wrap_err_with(|| format!("Failed to parse prefund balance `{value}` as hex wei"));
    }

    U256::from_str(value)
        .wrap_err_with(|| format!("Failed to parse prefund balance `{value}` as decimal wei"))
}

fn current_unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{ChainIds, DEFAULT_PREFUND_BALANCE, DeployerConfig};

    #[test]
    fn parses_toml_config() {
        let config: DeployerConfig = toml::from_str(
            r#"
l1_chain_id = 1337
l2_chain_id = 8453
slot_duration = 4
l2_base_v1_block = 20
"#,
        )
        .expect("toml config should parse");

        assert_eq!(config.l1_chain_id, Some(1337));
        assert_eq!(config.l2_chain_id, Some(8453));
        assert_eq!(config.slot_duration, Some(4));
        assert_eq!(config.l2_base_v1_block, Some(20));
    }

    #[test]
    fn parses_json_config() {
        let config: DeployerConfig = serde_json::from_str(
            r#"{
  "l1_chain_id": 901337,
  "l2_chain_id": 84538453,
  "prefund_balance": "0x10"
}"#,
        )
        .expect("json config should parse");

        assert_eq!(config.l1_chain_id, Some(901337));
        assert_eq!(config.l2_chain_id, Some(84538453));
        assert_eq!(config.prefund_balance.as_deref(), Some("0x10"));
    }

    #[test]
    fn resolves_defaults_and_random_chain_ids() {
        let resolved = DeployerConfig::default().resolve(None).expect("config should resolve");

        assert!((1_300_000..=1_399_999).contains(&resolved.l1_chain_id));
        assert!((84_530_000..=84_539_999).contains(&resolved.l2_chain_id));
        assert_ne!(resolved.l1_chain_id, resolved.l2_chain_id);
        assert_eq!(format!("{:#x}", resolved.prefund_balance), DEFAULT_PREFUND_BALANCE);
    }

    #[test]
    fn reuses_existing_chain_ids_when_present() {
        let tempdir = TempDir::new().expect("tempdir should be created");
        let output_dir = tempdir.path().to_path_buf();
        let existing = ChainIds { l1_chain_id: 1337, l2_chain_id: 84538453 };
        fs::write(
            output_dir.join("chain-ids.json"),
            serde_json::to_string_pretty(&existing).expect("chain ids should serialize"),
        )
        .expect("chain ids should be written");

        let resolved = DeployerConfig::default()
            .resolve_with_l1_chain_id(Some(output_dir.clone()), existing.l1_chain_id)
            .expect("config should resolve");

        assert_eq!(resolved.l1_chain_id, existing.l1_chain_id);
        assert_eq!(resolved.l2_chain_id, existing.l2_chain_id);
    }
}
