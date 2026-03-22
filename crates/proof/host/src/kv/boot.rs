use alloy_primitives::B256;
use base_proof::{
    INTERMEDIATE_BLOCK_INTERVAL_KEY, L1_CONFIG_KEY, L1_HEAD_KEY, L2_CHAIN_ID_KEY,
    L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY, L2_OUTPUT_ROOT_KEY, L2_ROLLUP_CONFIG_KEY,
    PROPOSER_KEY,
};
use base_proof_preimage::PreimageKey;

use crate::{HostConfig, KeyValueStore, Result};

/// A read-only key-value store that serves boot parameters from [`HostConfig`].
#[derive(Debug)]
pub struct BootKeyValueStore {
    cfg: HostConfig,
}

impl BootKeyValueStore {
    /// Creates a new [`BootKeyValueStore`].
    pub const fn new(cfg: HostConfig) -> Self {
        Self { cfg }
    }
}

impl KeyValueStore for BootKeyValueStore {
    fn get(&self, key: B256) -> Option<Vec<u8>> {
        let preimage_key = PreimageKey::try_from(*key).ok()?;
        match preimage_key.key_value() {
            L1_HEAD_KEY => Some(self.cfg.request.l1_head.to_vec()),
            L2_OUTPUT_ROOT_KEY => Some(self.cfg.request.agreed_l2_output_root.to_vec()),
            L2_CLAIM_KEY => Some(self.cfg.request.claimed_l2_output_root.to_vec()),
            L2_CLAIM_BLOCK_NUMBER_KEY => {
                Some(self.cfg.request.claimed_l2_block_number.to_be_bytes().to_vec())
            }
            L2_CHAIN_ID_KEY => Some(self.cfg.prover.l2_chain_id.to_be_bytes().to_vec()),
            L2_ROLLUP_CONFIG_KEY => {
                let serialized = serde_json::to_vec(&self.cfg.prover.rollup_config).ok()?;
                Some(serialized)
            }
            L1_CONFIG_KEY => {
                let serialized = serde_json::to_vec(&self.cfg.prover.l1_config).ok()?;
                Some(serialized)
            }
            PROPOSER_KEY => Some(self.cfg.request.proposer.to_vec()),
            INTERMEDIATE_BLOCK_INTERVAL_KEY => {
                Some(self.cfg.request.intermediate_block_interval.to_be_bytes().to_vec())
            }
            _ => None,
        }
    }

    fn set(&mut self, _: B256, _: Vec<u8>) -> Result<()> {
        Err(crate::HostError::Custom("BootKeyValueStore is read-only".into()))
    }
}
