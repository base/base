//! Public value types for the R9 victim at-most-once claim store.
//!
//! These types are keyless, red-line-safe provenance carriers: they hold claim
//! identity and store-identity bytes only. No key material, transaction
//! egress, or network surface is expressed or reachable from any type here.

use alloy_primitives::B256;

/// Fixed 32-byte digest of the campaign parameters that motivated a backrun.
///
/// This is **provenance only**: it is stored alongside a claim but is not part
/// of the uniqueness key. Two campaigns that target the same victim resolve to
/// the same global claim (the second is rejected), regardless of `CampaignId`.
/// Defined here because the fork baseline has no `CampaignId`; downstream
/// arming code (`B3-arm`) references this exact type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CampaignId([u8; 32]);

impl CampaignId {
    /// Wraps 32 raw digest bytes as a `CampaignId`.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the raw 32-byte digest.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Identity of the durable claim store that issued a proof.
///
/// The value is the store's `creation_nonce` (32 bytes) written once at
/// bootstrap by the OS CSPRNG. It exists to bind a claim proof to a specific,
/// owner-attested store instance (the identity trust-chain): a claim minted by
/// a rogue store carries a different nonce and is rejected by the downstream
/// consumer that checks `claim.store_identity() == deploy.r9_store_identity()`.
///
/// `Copy` by design — it is an identity value, never a path or handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StoreIdentity([u8; 32]);

impl StoreIdentity {
    /// Wraps a 32-byte creation nonce as a `StoreIdentity`.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the raw 32-byte creation nonce.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Durable, unforgeable proof that a specific victim was claimed exactly once.
///
/// The type deliberately has **no derive** and is **non-`Clone`**: a
/// `VictimClaim` can only be minted by [`crate::VictimClaimStore`] on the
/// commit-succeeded path (`new_internal` is `pub(super)`), which is the proof
/// contract consumed downstream. It carries the store identity so the consumer
/// can verify the proof originated from the approved store.
pub struct VictimClaim {
    chain_id: u64,
    victim_tx_hash: B256,
    campaign_id: CampaignId,
    store_identity: StoreIdentity,
}

impl VictimClaim {
    /// Mints a claim proof. `pub(super)` so only the sibling store module can
    /// call it, and only after a durable commit — proofs are unforgeable.
    pub(super) const fn new_internal(
        chain_id: u64,
        victim_tx_hash: B256,
        campaign_id: CampaignId,
        store_identity: StoreIdentity,
    ) -> Self {
        Self { chain_id, victim_tx_hash, campaign_id, store_identity }
    }

    /// The victim transaction hash this proof covers.
    pub const fn victim_tx_hash(&self) -> B256 {
        self.victim_tx_hash
    }

    /// The chain id this proof covers.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The provenance campaign digest recorded with the claim.
    pub const fn campaign_id(&self) -> CampaignId {
        self.campaign_id
    }

    /// The identity of the store that minted this proof.
    pub const fn store_identity(&self) -> StoreIdentity {
        self.store_identity
    }
}

// Manual `Debug` (not a derive) so `VictimClaim` stays non-`Clone` and
// derive-free while still being printable inside `ClaimResult`.
impl core::fmt::Debug for VictimClaim {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VictimClaim")
            .field("chain_id", &self.chain_id)
            .field("victim_tx_hash", &self.victim_tx_hash)
            .field("campaign_id", &self.campaign_id)
            .field("store_identity", &self.store_identity)
            .finish()
    }
}

/// Outcome of a claim attempt.
#[derive(Debug)]
pub enum ClaimResult {
    /// The victim was newly and durably claimed; carries the proof.
    Claimed(VictimClaim),
    /// The victim was already claimed (globally, by any campaign). No proof is
    /// issued and no submission may proceed for this victim.
    AlreadyClaimed,
}

/// Failure modes of the claim store. Every variant is a hard, fail-closed
/// signal for the consumer: on any error no proof exists, so no inclusion
/// submission may proceed (the consumer latches a kill/suppression).
#[derive(Debug, thiserror::Error)]
pub enum ClaimStoreError {
    /// Filesystem / storage I/O failure (open, lock file, `/dev/urandom`, etc.).
    #[error("victim claim store io error: {0}")]
    Io(String),
    /// A stored record failed structural validation (bad length or unknown
    /// format version). Fail-closed: the store is treated as untrustworthy.
    #[error("victim claim store corruption: {0}")]
    Corruption(String),
    /// The write transaction did not durably commit. The outcome is unknown
    /// (the record may or may not be persisted) so no proof is issued and the
    /// consumer must not retry inclusion.
    #[error("victim claim commit failed (outcome unknown): {0}")]
    CommitFailed(String),
    /// Another process already holds the exclusive singleton-writer lock for
    /// this store path. The second opener is refused (fail-closed).
    #[error("victim claim store is already held by the singleton writer")]
    NotSingletonWriter,
    /// The store's metadata is absent or its creation nonce does not match the
    /// expected (owner-attested) identity. Prevents auto-recreation of a lost
    /// or empty store from being mistaken for a legitimate zero-claim store.
    #[error("victim claim store identity mismatch")]
    StoreIdentityMismatch,
}
