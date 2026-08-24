//! Derivation of the human-readable `network` facet from an L2 chain ID.

/// Resolves the `network` wire value reports carry.
///
/// The facet is the label dashboards group by and the archive partitions on, so it must be a
/// name rather than a number: `l2_chain_id` already carries the number, and a partition key of
/// `8453` tells a reader nothing that `mainnet` does not tell them better.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkName;

impl NetworkName {
    /// Base mainnet.
    pub const MAINNET_CHAIN_ID: u64 = 8453;
    /// Base Sepolia.
    pub const SEPOLIA_CHAIN_ID: u64 = 84532;
    /// The local devnet the `etc/docker` stack brings up.
    pub const DEVNET_CHAIN_ID: u64 = 84538453;

    /// Returns the network name for `l2_chain_id`.
    ///
    /// An unrecognized chain reports `chain-<id>` rather than the bare number, so a new network
    /// is legible in a dashboard before anyone adds it here, and never collides with a name.
    pub fn for_chain_id(l2_chain_id: u64) -> String {
        match l2_chain_id {
            Self::MAINNET_CHAIN_ID => "mainnet".to_string(),
            Self::SEPOLIA_CHAIN_ID => "sepolia".to_string(),
            Self::DEVNET_CHAIN_ID => "devnet".to_string(),
            other => format!("chain-{other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_chains_resolve_to_names() {
        assert_eq!(NetworkName::for_chain_id(8453), "mainnet");
        assert_eq!(NetworkName::for_chain_id(84532), "sepolia");
        assert_eq!(NetworkName::for_chain_id(84538453), "devnet");
    }

    #[test]
    fn test_unknown_chain_is_labelled_not_bare() {
        assert_eq!(
            NetworkName::for_chain_id(1234),
            "chain-1234",
            "an unnamed chain must still be distinguishable from a named one"
        );
    }
}
