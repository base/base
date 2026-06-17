//! L1 transaction format selection for the derivation-pipeline reader.

use alloy_genesis::ChainConfig;

/// The transaction format of the L1 chain the derivation pipeline reads from.
///
/// A `Base` L1 carries deposit (`0x7E`) and EIP-8130 (`0x7D`) transactions (and the receipts
/// mirroring them) the default Ethereum envelopes cannot deserialize. Derived from the committed
/// L1 config's `optimism` field.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, derive_more::Display, derive_more::FromStr,
)]
pub enum L1TxFormat {
    /// An Ethereum-format L1 chain (alloy's standard envelopes, blob DA).
    #[display("ethereum")]
    #[default]
    Ethereum,
    /// A Base/OP-format L1 chain (deposit/EIP-8130 transactions, calldata DA).
    #[display("base")]
    Base,
}

impl L1TxFormat {
    /// Derives the parent-chain transaction format from an L1 config.
    ///
    /// This only selects decoding for bytes already committed by L1 header roots.
    /// Detection uses the `"optimism"` extra field when present (e.g. configs deserialized
    /// from JSON) and falls back to a known chain-ID allowlist for statically-constructed
    /// configs that cannot set `extra_fields` without a `serde_json` dependency.
    pub fn from_l1_config(cfg: &ChainConfig) -> Self {
        if cfg.extra_fields.contains_key("optimism") || Self::is_op_stack_chain(cfg.chain_id) {
            Self::Base
        } else {
            Self::Ethereum
        }
    }

    /// Returns `true` for chain IDs of known OP Stack L2s used as settlement layers.
    const fn is_op_stack_chain(chain_id: u64) -> bool {
        matches!(chain_id, 8453 | 84532)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use alloy_genesis::ChainConfig;
    use serde_json::json;

    use super::*;

    #[test]
    fn optimism_extra_field_derives_base_format() {
        let mut cfg = ChainConfig::default();
        cfg.extra_fields.insert("optimism".to_string(), json!({}));

        assert_eq!(L1TxFormat::from_l1_config(&cfg), L1TxFormat::Base);
    }

    #[test]
    fn missing_optimism_extra_field_derives_ethereum_format() {
        let cfg = ChainConfig::default();

        assert_eq!(L1TxFormat::from_l1_config(&cfg), L1TxFormat::Ethereum);
    }

    #[test]
    fn base_sepolia_chain_id_derives_base_format() {
        let mut cfg = ChainConfig::default();
        cfg.chain_id = 84532;

        assert_eq!(L1TxFormat::from_l1_config(&cfg), L1TxFormat::Base);
    }

    #[test]
    fn base_mainnet_chain_id_derives_base_format() {
        let mut cfg = ChainConfig::default();
        cfg.chain_id = 8453;

        assert_eq!(L1TxFormat::from_l1_config(&cfg), L1TxFormat::Base);
    }
}
