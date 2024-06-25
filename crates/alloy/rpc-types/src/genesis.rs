//! OP types for genesis data.

/// Genesis info for Optimism.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimismGenesisInfo {
    /// Bedrock block number
    pub bedrock_block: Option<u64>,
    /// regolith hardfork timestamp
    pub regolith_time: Option<u64>,
    /// canyon hardfork timestamp
    pub canyon_time: Option<u64>,
    /// ecotone hardfork timestamp
    pub ecotone_time: Option<u64>,
    /// fjord hardfork timestamp
    pub fjord_time: Option<u64>,
}

/// Additional base fee info for Optimism.
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
