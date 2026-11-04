//! Bootnode list parsing shared by the consensus `node` and `bootnode` commands.

use base_consensus_disc::LinkEntry;
use base_consensus_peers::{BootNode, BootNodes};

/// Bootnodes from the command line, split into fixed bootnodes and EIP-1459 ENR trees.
#[derive(Debug, Clone, Default)]
pub struct ConfiguredBootNodes {
    /// ENRs and node records.
    pub bootnodes: BootNodes,
    /// ENR trees (`enrtree://<pubkey>@<domain>`) for discovery to sync in the background.
    pub enr_trees: Vec<LinkEntry>,
}

impl ConfiguredBootNodes {
    /// Prefix of an EIP-1459 ENR tree link in the bootnode list.
    pub const ENR_TREE_PREFIX: &str = "enrtree://";

    /// Parses bootnode strings. A malformed entry is an error.
    ///
    /// `enrtree://` entries are parsed without network access. Other entries go through
    /// [`BootNode::parse_bootnode`], which may resolve a hostname with a blocking DNS lookup.
    pub fn parse<S: AsRef<str>>(raw: impl IntoIterator<Item = S>) -> eyre::Result<Self> {
        let mut configured = Self::default();
        for bootnode in raw {
            let bootnode = bootnode.as_ref();
            if bootnode.starts_with(Self::ENR_TREE_PREFIX) {
                let tree = bootnode
                    .parse::<LinkEntry>()
                    .map_err(|e| eyre::eyre!("Failed to parse ENR tree '{bootnode}': {e}"))?;
                configured.enr_trees.push(tree);
            } else {
                let node = BootNode::parse_bootnode(bootnode)
                    .map_err(|e| eyre::eyre!("Failed to parse bootnode '{bootnode}': {e}"))?;
                configured.bootnodes.0.push(node);
            }
        }
        Ok(configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4";
    const TREE: &str =
        "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@nodes.example.org";

    #[test]
    fn splits_enr_trees_from_bootnodes() {
        let raw = [format!("enode://{ID}@34.65.205.244:30305"), TREE.to_string()];

        let configured = ConfiguredBootNodes::parse(&raw).unwrap();

        assert_eq!(configured.enr_trees.len(), 1);
        assert_eq!(configured.enr_trees[0].domain, "nodes.example.org");
        assert_eq!(configured.enr_trees[0].to_string(), TREE);
        assert_eq!(configured.bootnodes.len(), 1);
    }

    #[test]
    fn invalid_enr_tree_errors() {
        let err = ConfiguredBootNodes::parse(["enrtree://not-a-key@nodes.example.org"])
            .expect_err("invalid tree should fail");

        assert!(err.to_string().contains("Failed to parse ENR tree"));
    }

    #[test]
    fn invalid_bootnode_errors() {
        let err = ConfiguredBootNodes::parse(["enr:invalid"]).expect_err("should fail");

        assert!(err.to_string().contains("Failed to parse bootnode 'enr:invalid'"));
    }
}
