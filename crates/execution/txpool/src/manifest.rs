//! Captured authorization read-set for stateless intra-block revalidation.
//!
//! At mempool validation the full EIP-8130 authorizer runs once against live
//! state. A [`WatchManifest`] snapshots the exact `AccountConfiguration` storage
//! slots that authorization read, together with the value each held. Because
//! authentication (signature recovery / dispatch) is stateless — it reads the
//! transaction bytes and does crypto, never storage — this slot set plus the
//! payer-balance and expiry predicates is the *complete* state dependency of the
//! transaction's validity. If none of them changed, re-running authentication is
//! provably redundant.
//!
//! The manifest is captured drift-free: it is exactly what the authorizer read
//! (including the delegate account's slots on the delegate path, which the
//! hand-derived [`crate::WatchSet`] cannot see), because it is recorded at the
//! storage layer rather than re-derived. It is intended to be consumed by the
//! builder for a cheap, stateless intra-block validity check. See
//! `MEMPOOL_HARDENING.md` §13.

use alloy_primitives::{Address, U256};

/// A single `AccountConfiguration` storage slot an EIP-8130 transaction's
/// authorization read, with the base-state value it held at validation time.
///
/// Equality predicate: any change to the slot invalidates the transaction, since
/// the authorizer's output is a pure function of the slots it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSlot {
    /// Contract whose storage slot was read (the `AccountConfiguration` system
    /// contract for actor-config / account-state / policy slots).
    pub address: Address,
    /// Storage slot key within `address`.
    pub slot: U256,
    /// Value the slot held when the transaction was authorized.
    pub expected: U256,
}

/// The complete, drift-free snapshot of the state an EIP-8130 transaction's
/// authorization depended on, captured once at mempool validation.
///
/// Holds the authorization read-set (`config_slots`, equality-predicated) plus
/// the payer-balance threshold and effective expiry. The nonce surface is not
/// duplicated here; it is carried by the [`crate::WatchSet`] and re-checked
/// cheaply alongside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchManifest {
    config_slots: Vec<ConfigSlot>,
    payer: Address,
    payer_max_cost: U256,
    effective_expiry: u64,
}

impl WatchManifest {
    /// Builds a manifest from a captured authorization read-set and the payer /
    /// expiry predicates.
    #[must_use]
    pub const fn new(
        config_slots: Vec<ConfigSlot>,
        payer: Address,
        payer_max_cost: U256,
        effective_expiry: u64,
    ) -> Self {
        Self { config_slots, payer, payer_max_cost, effective_expiry }
    }

    /// The `AccountConfiguration` slots (with expected values) the transaction's
    /// authorization read.
    #[must_use]
    pub fn config_slots(&self) -> &[ConfigSlot] {
        &self.config_slots
    }

    /// The account whose balance funds the transaction.
    #[must_use]
    pub const fn payer(&self) -> Address {
        self.payer
    }

    /// The maximum cost the transaction can charge the payer (threshold the
    /// payer balance must still satisfy).
    #[must_use]
    pub const fn payer_max_cost(&self) -> U256 {
        self.payer_max_cost
    }

    /// The effective expiry (min of the transaction's own expiry and the
    /// sender/payer key expiries); `u64::MAX` means unbounded.
    #[must_use]
    pub const fn effective_expiry(&self) -> u64 {
        self.effective_expiry
    }

    /// Whether the manifest recorded no config slots (a transaction whose
    /// authorization read nothing from `AccountConfiguration`).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.config_slots.is_empty()
    }
}
