//! Base Sepolia (chain ID 84532) as an OP-stack L1 parent.
//!
//! Privacy L3 devnet (84534) derives batches from Base Sepolia blocks, so the
//! nitro prover host needs an execution-layer chain config for 84532 — not
//! Ethereum Sepolia (11155111).

use alloy_genesis::{ChainConfig, Genesis};
use spin::Lazy;

/// Base Sepolia OP-stack L1 configuration builder.
#[derive(Debug, Clone, Copy)]
pub struct BaseSepolia;

static L1_CONFIG: Lazy<ChainConfig> = Lazy::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../../res/l1/base_sepolia.json"))
        .expect("valid Base Sepolia L1 genesis JSON");
    genesis.config
});

impl BaseSepolia {
    /// Returns the Base Sepolia execution-layer [`ChainConfig`] for L1 proving.
    pub fn l1_config() -> ChainConfig {
        L1_CONFIG.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_sepolia_l1_config_chain_id() {
        assert_eq!(BaseSepolia::l1_config().chain_id, 84532);
    }

    #[test]
    fn base_sepolia_l1_config_has_op_hardforks() {
        let config = BaseSepolia::l1_config();
        assert!(config.extra_fields.contains_key("bedrockBlock"));
        assert!(config.extra_fields.contains_key("canyonTime"));
        assert!(config.extra_fields.contains_key("jovianTime"));
        assert!(config.extra_fields.contains_key("optimism"));
    }
}
