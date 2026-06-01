//! Deployment artifact types (genesis, rollup config, addresses).

use std::path::Path;

use eyre::{Result, WrapErr};
use serde_json::Value;

const L2_GENESIS_FILE: &str = "genesis.json";
const ROLLUP_CONFIG_FILE: &str = "rollup.json";
const L1_ADDRESSES_FILE: &str = "l1-addresses.json";

/// Artifacts emitted by the L2 contract deployment.
#[derive(Debug, Clone)]
pub struct DeploymentArtifacts {
    /// L2 genesis configuration.
    pub l2_genesis: Value,
    /// Rollup configuration.
    pub rollup_config: Value,
    /// L1 contract addresses for the deployment.
    pub l1_addresses: Value,
}

impl DeploymentArtifacts {
    /// Returns true when all expected artifacts are present.
    pub fn exists_in(dir: impl AsRef<Path>) -> bool {
        let dir = dir.as_ref();
        dir.join(L2_GENESIS_FILE).exists()
            && dir.join(ROLLUP_CONFIG_FILE).exists()
            && dir.join(L1_ADDRESSES_FILE).exists()
    }

    /// Loads deployment artifacts from the output directory.
    pub fn load_from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let l2_genesis = read_json(&dir.join(L2_GENESIS_FILE))?;
        let rollup_config = read_json(&dir.join(ROLLUP_CONFIG_FILE))?;
        let l1_addresses = read_json(&dir.join(L1_ADDRESSES_FILE))?;

        Ok(Self { l2_genesis, rollup_config, l1_addresses })
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let contents = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("Failed to read artifact at {}", path.display()))?;
    let value = serde_json::from_str(&contents)
        .wrap_err_with(|| format!("Failed to parse JSON at {}", path.display()))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use base_common_genesis::RollupConfig;

    /// A rollup.json fixture matching the format produced by the devnet deployer
    /// (setup-l2.sh / assemble-rollup-config.sh). Uses devnet-specific parameter
    /// values: l1_chain_id=1337, l2_chain_id=84538453, all hardforks at 0.
    const DEVNET_ROLLUP_JSON: &str = r#"
    {
      "genesis": {
        "l1": {
          "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54",
          "number": 10
        },
        "l2": {
          "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631",
          "number": 0
        },
        "l2_time": 1725557164,
        "system_config": {
          "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
          "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
          "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
          "gasLimit": 30000000
        }
      },
      "block_time": 2,
      "max_sequencer_drift": 600,
      "seq_window_size": 3600,
      "channel_timeout": 300,
      "l1_chain_id": 1337,
      "l2_chain_id": 84538453,
      "regolith_time": 0,
      "canyon_time": 0,
      "delta_time": 0,
      "ecotone_time": 0,
      "fjord_time": 0,
      "granite_time": 0,
      "holocene_time": 0,
      "isthmus_time": 0,
      "jovian_time": 0,
      "batch_inbox_address": "0xff00000000000000000000000000000000042069",
      "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
      "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
      "protocol_versions_address": "0x0000000000000000000000000000000000000000",
      "chain_op_config": {
        "eip1559Elasticity": 6,
        "eip1559Denominator": 50,
        "eip1559DenominatorCanyon": 250
      }
    }
    "#;

    /// Validates that the devnet rollup.json format deserializes into [`RollupConfig`]
    /// successfully. This catches schema drift between the shell-produced JSON and the
    /// Rust struct (which uses `deny_unknown_fields`).
    #[test]
    fn test_devnet_rollup_json_deserializes() {
        let config: RollupConfig = serde_json::from_str(DEVNET_ROLLUP_JSON)
            .expect("devnet rollup.json must deserialize into RollupConfig");

        assert_eq!(config.l1_chain_id, 1337);
        assert_eq!(config.l2_chain_id.id(), 84538453);
        assert_eq!(config.block_time, 2);
        assert_eq!(config.hardforks.regolith_time, Some(0));
        assert_eq!(config.hardforks.canyon_time, Some(0));
        assert_eq!(config.hardforks.delta_time, Some(0));
        assert_eq!(config.hardforks.ecotone_time, Some(0));
        assert_eq!(config.hardforks.fjord_time, Some(0));
        assert_eq!(config.hardforks.granite_time, Some(0));
        assert_eq!(config.hardforks.holocene_time, Some(0));
        assert_eq!(config.hardforks.isthmus_time, Some(0));
        assert_eq!(config.hardforks.jovian_time, Some(0));
    }

    /// Verifies that `deny_unknown_fields` on [`RollupConfig`] rejects extra fields.
    /// This protects against the shell script emitting fields that silently get ignored.
    #[test]
    fn test_devnet_rollup_json_rejects_unknown_fields() {
        let with_extra_field: &str = r#"
        {
          "genesis": {
            "l1": {
              "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54",
              "number": 10
            },
            "l2": {
              "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631",
              "number": 0
            },
            "l2_time": 1725557164,
            "system_config": {
              "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
              "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
              "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
              "gasLimit": 30000000
            }
          },
          "block_time": 2,
          "max_sequencer_drift": 600,
          "seq_window_size": 3600,
          "channel_timeout": 300,
          "l1_chain_id": 1337,
          "l2_chain_id": 84538453,
          "regolith_time": 0,
          "canyon_time": 0,
          "delta_time": 0,
          "ecotone_time": 0,
          "fjord_time": 0,
          "batch_inbox_address": "0xff00000000000000000000000000000000042069",
          "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
          "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
          },
          "superchain_time": 999
        }
        "#;

        let err = serde_json::from_str::<RollupConfig>(with_extra_field)
            .expect_err("extra fields must cause deserialization failure");
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }
}
