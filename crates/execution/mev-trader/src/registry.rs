use alloy_primitives::{Address, B256, U256};

/// One registry-audited state key that victim execution may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditedWriteKey {
    /// An account balance write.
    AccountBalance {
        /// Account whose balance may change.
        address: Address,
        /// Digest of the audit evidence authorizing the key.
        evidence_digest: B256,
    },
    /// An account nonce write.
    AccountNonce {
        /// Account whose nonce may change.
        address: Address,
        /// Digest of the audit evidence authorizing the key.
        evidence_digest: B256,
    },
    /// A contract storage write.
    Storage {
        /// Contract whose storage may change.
        address: Address,
        /// Exact storage slot that may change.
        slot: U256,
        /// Digest of the audit evidence authorizing the key.
        evidence_digest: B256,
    },
}

impl AuditedWriteKey {
    /// Returns the account or contract address owned by this key.
    pub const fn address(&self) -> Address {
        match self {
            Self::AccountBalance { address, .. }
            | Self::AccountNonce { address, .. }
            | Self::Storage { address, .. } => *address,
        }
    }

    /// Returns the storage slot only for storage writes.
    pub const fn slot(&self) -> Option<U256> {
        match self {
            Self::Storage { slot, .. } => Some(*slot),
            Self::AccountBalance { .. } | Self::AccountNonce { .. } => None,
        }
    }

    /// Returns the audit evidence digest.
    pub const fn evidence_digest(&self) -> B256 {
        match self {
            Self::AccountBalance { evidence_digest, .. }
            | Self::AccountNonce { evidence_digest, .. }
            | Self::Storage { evidence_digest, .. } => *evidence_digest,
        }
    }
}
