use alloy_primitives::keccak256;
use base_execution_network_wire::{NodeRecord, PeerId};

/// The key type for the table.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct NodeKey(pub PeerId);

impl From<PeerId> for NodeKey {
    fn from(value: PeerId) -> Self {
        Self(value)
    }
}

impl From<NodeKey> for crate::Key<NodeKey> {
    fn from(value: NodeKey) -> Self {
        let hash = keccak256(value.0.as_slice());
        Self::new_raw(value, hash.0.into())
    }
}

impl From<&NodeRecord> for NodeKey {
    fn from(node: &NodeRecord) -> Self {
        Self(node.id)
    }
}

impl NodeKey {
    /// Converts a `PeerId` into the required `Key` type for the table
    #[inline]
    pub fn kad_key(node: PeerId) -> crate::Key<NodeKey> {
        crate::Key::from(NodeKey::from(node))
    }
}
