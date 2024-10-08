#![allow(missing_docs)]
//! OP rollup config types.

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

// https://github.com/ethereum-optimism/optimism/blob/c7ad0ebae5dca3bf8aa6f219367a95c15a15ae41/op-service/eth/types.go#L371
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemConfig {
    pub batcher_addr: Address,
    pub overhead: B256,
    pub scalar: B256,
    pub gas_limit: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genesis {
    pub l1: BlockNumHash,
    pub l2: BlockNumHash,
    pub l2_time: u64,
    pub system_config: SystemConfig,
}

// <https://github.com/ethereum-optimism/optimism/blob/77c91d09eaa44d2c53bec60eb89c5c55737bc325/op-node/rollup/types.go#L66>
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupConfig {
    pub genesis: Genesis,
    pub block_time: u64,
    pub max_sequencer_drift: u64,
    pub seq_window_size: u64,

    #[serde(rename = "channel_timeout")]
    pub channel_timeout_bedrock: u64,
    pub channel_timeout_granite: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l1_chain_id: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_chain_id: Option<u128>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regolith_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canyon_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecotone_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fjord_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granite_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interop_time: Option<u64>,
    pub batch_inbox_address: Address,
    pub deposit_contract_address: Address,
    pub l1_system_config_address: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_versions_address: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub da_challenge_address: Option<Address>,
    pub da_challenge_window: u64,
    pub da_resolve_window: u64,
    pub use_plasma: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rollup_config() {
        let s = r#"{"genesis":{"l1":{"hash":"0x438335a20d98863a4c0c97999eb2481921ccd28553eac6f913af7c12aec04108", "number": 424242 },"l2":{"hash":"0xdbf6a80fef073de06add9b0d14026d6e5a86c85f6d102c36d3d8e9cf89c2afd3", "number": 1337 },"l2_time":1686068903,"system_config":{"batcherAddr":"0x6887246668a3b87f54deb3b94ba47a6f63f32985","overhead":"0x00000000000000000000000000000000000000000000000000000000000000bc","scalar":"0x00000000000000000000000000000000000000000000000000000000000a6fe0","gasLimit":30000000}},"block_time":2,"max_sequencer_drift":600,"seq_window_size":3600,"channel_timeout":300,"channel_timeout_granite":50,"l1_chain_id":1,"l2_chain_id":10,"regolith_time":0,"canyon_time":1704992401,"delta_time":1708560000,"ecotone_time":1710374401,"batch_inbox_address":"0xff00000000000000000000000000000000000010","deposit_contract_address":"0xbeb5fc579115071764c7423a4f12edde41f106ed","l1_system_config_address":"0x229047fed2591dbec1ef1118d64f7af3db9eb290","protocol_versions_address":"0x8062abc286f5e7d9428a0ccb9abd71e50d93b935","da_challenge_address":"0x0000000000000000000000000000000000000000","da_challenge_window":0,"da_resolve_window":0,"use_plasma":false}"#;

        let deserialize = serde_json::from_str::<RollupConfig>(s).unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(s).unwrap(),
            serde_json::to_value(&deserialize).unwrap()
        );
    }
}
