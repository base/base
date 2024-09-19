//! OP types for genesis data.

use alloy_serde::OtherFields;
use serde::de::Error;

/// Container type for all Optimism specific fields in a genesis file.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimismChainInfo {
    /// Genesis information
    pub genesis_info: Option<OptimismGenesisInfo>,
    /// Base fee information
    pub base_fee_info: Option<OptimismBaseFeeInfo>,
}

impl OptimismChainInfo {
    /// Extracts the Optimism specific fields from a genesis file. These fields are expected to be
    /// contained in the `genesis.config` under `extra_fields` property.
    pub fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
}

impl TryFrom<&OtherFields> for OptimismChainInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        let genesis_info = OptimismGenesisInfo::try_from(others).ok();
        let base_fee_info = OptimismBaseFeeInfo::try_from(others).ok();

        Ok(OptimismChainInfo { genesis_info, base_fee_info })
    }
}

/// The Optimism-specific genesis block specification.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimismGenesisInfo {
    /// bedrock block number
    pub bedrock_block: Option<u64>,
    /// regolith hardfork timestamp
    pub regolith_time: Option<u64>,
    /// canyon hardfork timestamp
    pub canyon_time: Option<u64>,
    /// ecotone hardfork timestamp
    pub ecotone_time: Option<u64>,
    /// fjord hardfork timestamp
    pub fjord_time: Option<u64>,
    /// granite hardfork timestamp
    pub granite_time: Option<u64>,
    /// holocene hardfork timestamp
    pub holocene_time: Option<u64>,
}

impl OptimismGenesisInfo {
    /// Extract the Optimism-specific genesis info from a genesis file.
    pub fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
}

impl TryFrom<&OtherFields> for OptimismGenesisInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        others.deserialize_as()
    }
}

/// The Optimism-specific base fee specification.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimismBaseFeeInfo {
    /// EIP-1559 elasticity
    pub eip1559_elasticity: Option<u64>,
    /// EIP-1559 denominator
    pub eip1559_denominator: Option<u64>,
    /// EIP-1559 denominator after canyon
    pub eip1559_denominator_canyon: Option<u64>,
}

impl OptimismBaseFeeInfo {
    /// Extracts the Optimism base fee info by looking for the `optimism` key. It is intended to be
    /// parsed from a genesis file.
    pub fn extract_from(others: &OtherFields) -> Option<Self> {
        Self::try_from(others).ok()
    }
}

impl TryFrom<&OtherFields> for OptimismBaseFeeInfo {
    type Error = serde_json::Error;

    fn try_from(others: &OtherFields) -> Result<Self, Self::Error> {
        if let Some(Ok(optimism_base_fee_info)) =
            others.get_deserialized::<OptimismBaseFeeInfo>("optimism")
        {
            Ok(optimism_base_fee_info)
        } else {
            Err(serde_json::Error::missing_field("optimism"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_optimism_genesis_info() {
        let genesis_info = r#"
        {
          "bedrockBlock": 10,
          "regolithTime": 12,
          "canyonTime": 0,
          "ecotoneTime": 0
        }
        "#;

        let others: OtherFields = serde_json::from_str(genesis_info).unwrap();
        let genesis_info = OptimismGenesisInfo::extract_from(&others).unwrap();

        assert_eq!(
            genesis_info,
            OptimismGenesisInfo {
                bedrock_block: Some(10),
                regolith_time: Some(12),
                canyon_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: None,
                granite_time: None,
                holocene_time: None,
            }
        );
    }

    #[test]
    fn test_extract_optimism_base_fee_info() {
        let base_fee_info = r#"
        {
          "optimism": {
            "eip1559Elasticity": 0,
            "eip1559Denominator": 8,
            "eip1559DenominatorCanyon": 8
          }
        }
        "#;

        let others: OtherFields = serde_json::from_str(base_fee_info).unwrap();
        let base_fee_info = OptimismBaseFeeInfo::extract_from(&others).unwrap();

        assert_eq!(
            base_fee_info,
            OptimismBaseFeeInfo {
                eip1559_elasticity: Some(0),
                eip1559_denominator: Some(8),
                eip1559_denominator_canyon: Some(8),
            }
        );
    }

    #[test]
    fn test_extract_optimism_chain_info() {
        let chain_info = r#"
        {
          "bedrockBlock": 10,
          "regolithTime": 12,
          "canyonTime": 0,
          "ecotoneTime": 0,
          "optimism": {
            "eip1559Denominator": 8,
            "eip1559DenominatorCanyon": 8
          }
        }
        "#;

        let others: OtherFields = serde_json::from_str(chain_info).unwrap();
        let chain_info = OptimismChainInfo::extract_from(&others).unwrap();

        assert_eq!(
            chain_info,
            OptimismChainInfo {
                genesis_info: Some(OptimismGenesisInfo {
                    bedrock_block: Some(10),
                    regolith_time: Some(12),
                    canyon_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: None,
                    granite_time: None,
                    holocene_time: None,
                }),
                base_fee_info: Some(OptimismBaseFeeInfo {
                    eip1559_elasticity: None,
                    eip1559_denominator: Some(8),
                    eip1559_denominator_canyon: Some(8),
                }),
            }
        );

        let chain_info = OptimismChainInfo::try_from(&others).unwrap();

        assert_eq!(
            chain_info,
            OptimismChainInfo {
                genesis_info: Some(OptimismGenesisInfo {
                    bedrock_block: Some(10),
                    regolith_time: Some(12),
                    canyon_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: None,
                    granite_time: None,
                    holocene_time: None,
                }),
                base_fee_info: Some(OptimismBaseFeeInfo {
                    eip1559_elasticity: None,
                    eip1559_denominator: Some(8),
                    eip1559_denominator_canyon: Some(8),
                }),
            }
        );
    }

    #[test]
    fn test_extract_optimism_chain_info_no_base_fee() {
        let chain_info = r#"
        {
          "bedrockBlock": 10,
          "regolithTime": 12,
          "canyonTime": 0,
          "ecotoneTime": 0,
          "fjordTime": 0,
          "graniteTime": 0,
          "holoceneTime": 0
        }
        "#;

        let others: OtherFields = serde_json::from_str(chain_info).unwrap();
        let chain_info = OptimismChainInfo::extract_from(&others).unwrap();

        assert_eq!(
            chain_info,
            OptimismChainInfo {
                genesis_info: Some(OptimismGenesisInfo {
                    bedrock_block: Some(10),
                    regolith_time: Some(12),
                    canyon_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                }),
                base_fee_info: None,
            }
        );
    }
}
