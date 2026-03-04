use alloy_primitives::B256;
use base_proof::{
    L1_CONFIG_KEY, L1_HEAD_KEY, L2_CHAIN_ID_KEY, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY,
    L2_OUTPUT_ROOT_KEY, L2_ROLLUP_CONFIG_KEY,
};
use base_proof_preimage::PreimageKey;

use crate::{Host, KeyValueStore, Result};

/// A read-only key-value store backed by [`Host`] configuration.
#[derive(Debug)]
pub struct LocalInputs {
    cfg: Host,
}

impl LocalInputs {
    /// Create a new [`LocalInputs`] with the given [`Host`] config.
    pub const fn new(cfg: Host) -> Self {
        Self { cfg }
    }
}

impl KeyValueStore for LocalInputs {
    fn get(&self, key: B256) -> Option<Vec<u8>> {
        let preimage_key = PreimageKey::try_from(*key).ok()?;
        match preimage_key.key_value() {
            L1_HEAD_KEY => Some(self.cfg.l1_head.to_vec()),
            L2_OUTPUT_ROOT_KEY => Some(self.cfg.agreed_l2_output_root.to_vec()),
            L2_CLAIM_KEY => Some(self.cfg.claimed_l2_output_root.to_vec()),
            L2_CLAIM_BLOCK_NUMBER_KEY => {
                Some(self.cfg.claimed_l2_block_number.to_be_bytes().to_vec())
            }
            L2_CHAIN_ID_KEY => {
                Some(self.cfg.l2_chain_id.unwrap_or_default().to_be_bytes().to_vec())
            }
            L2_ROLLUP_CONFIG_KEY => {
                let rollup_config = self.cfg.read_rollup_config().ok()?;
                let serialized = serde_json::to_vec(&rollup_config).ok()?;
                Some(serialized)
            }
            L1_CONFIG_KEY => {
                let l1_config = self.cfg.read_l1_config().ok()?;
                let serialized = serde_json::to_vec(&l1_config).ok()?;
                Some(serialized)
            }
            _ => None,
        }
    }

    fn set(&mut self, _: B256, _: Vec<u8>) -> Result<()> {
        unreachable!("LocalInputs is read-only")
    }
}
