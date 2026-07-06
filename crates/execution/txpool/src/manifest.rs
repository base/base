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
use revm::Database;

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

    /// Conservative, stateless intra-block re-check of the transaction's captured
    /// validity predicates against current build-time state.
    ///
    /// This is the builder-side (Stage 1) fast-drop: because EIP-8130
    /// authentication is a pure function of the authorization read-set, if every
    /// recorded config slot still holds its expected value, the payer can still
    /// cover its maximum charge, and the effective expiry has not passed, then the
    /// authorizer would produce the identical result — so the transaction is still
    /// authorizable without re-running any signature recovery.
    ///
    /// Returns `Err(reason)` only when a predicate is *positively observed* to
    /// have failed, in which case the builder may drop the transaction ahead of
    /// execution. The check is deliberately **fail-open**: any storage/account
    /// read error (or a payer account that unexpectedly reads as absent) yields a
    /// pass, since the builder still runs full validation on survivors and a flaky
    /// read must never drop an otherwise-valid transaction. It therefore never
    /// admits an invalid transaction (that is execution's job) and never wrongly
    /// rejects a valid one.
    ///
    /// `now` is the timestamp of the block being built.
    /// # Note on `&mut DB` and cache effects
    ///
    /// `revm::Database::storage` requires `&mut self`, so the signature takes
    /// `&mut DB` even though `revalidate` does not write anything. The reads may
    /// be cached by the EVM's journal; this is correct for execution (the same
    /// value would be returned on any real execution read) but may include extra
    /// storage proofs in witness mode for slots that no actual execution would
    /// touch. This is a minor witness-size trade-off and does not affect
    /// correctness.
    pub fn revalidate<DB: Database>(&self, db: &mut DB, now: u64) -> Result<(), ManifestStale> {
        if self.effective_expiry != u64::MAX && now > self.effective_expiry {
            return Err(ManifestStale::Expired { deadline: self.effective_expiry, now });
        }

        for slot in &self.config_slots {
            // Fail-open on read error: only a positively observed change drops.
            if let Ok(current) = db.storage(slot.address, slot.slot)
                && current != slot.expected
            {
                return Err(ManifestStale::ConfigSlotChanged {
                    address: slot.address,
                    slot: slot.slot,
                });
            }
        }

        // Only drop on a positively observed shortfall from an existing account;
        // a read error or an unexpectedly-absent account fails open.
        if !self.payer_max_cost.is_zero()
            && let Ok(Some(info)) = db.basic(self.payer)
            && info.balance < self.payer_max_cost
        {
            return Err(ManifestStale::PayerUnderfunded {
                payer: self.payer,
                required: self.payer_max_cost,
                available: info.balance,
            });
        }

        Ok(())
    }
}

/// A positively observed reason an EIP-8130 transaction is stale at build time,
/// produced by [`WatchManifest::revalidate`]. Carries enough context for
/// structured tracing without high-cardinality metric labels (metric callers map
/// this to its coarse [`ManifestStale::cause`] category).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestStale {
    /// A watched `AccountConfiguration` slot no longer holds the value the
    /// authorization depended on, so authentication would differ.
    ConfigSlotChanged {
        /// Contract whose slot changed.
        address: Address,
        /// Storage slot that changed.
        slot: U256,
    },
    /// The payer can no longer cover the transaction's maximum charge.
    PayerUnderfunded {
        /// Account funding the transaction.
        payer: Address,
        /// Maximum charge the transaction can incur.
        required: U256,
        /// Balance currently available.
        available: U256,
    },
    /// The effective expiry deadline has passed at the build timestamp.
    Expired {
        /// Effective expiry deadline (unix seconds).
        deadline: u64,
        /// Build-time timestamp that passed the deadline.
        now: u64,
    },
}

impl ManifestStale {
    /// A coarse, low-cardinality category suitable for a metric label.
    #[must_use]
    pub const fn cause(&self) -> &'static str {
        match self {
            Self::ConfigSlotChanged { .. } => "config_slot",
            Self::PayerUnderfunded { .. } => "payer_balance",
            Self::Expired { .. } => "expiry",
        }
    }
}

#[cfg(test)]
mod tests {
    use revm::{database::InMemoryDB, state::AccountInfo};

    use super::*;

    const CONFIG: Address = Address::repeat_byte(0x11);
    const PAYER: Address = Address::repeat_byte(0x22);
    const SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);
    const EXPECTED: U256 = U256::from_limbs([42, 0, 0, 0]);

    fn db_with(slot_value: U256, payer_balance: u64) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            PAYER,
            AccountInfo { balance: U256::from(payer_balance), ..Default::default() },
        );
        db.insert_account_storage(CONFIG, SLOT, slot_value).unwrap();
        db
    }

    fn manifest() -> WatchManifest {
        WatchManifest::new(
            vec![ConfigSlot { address: CONFIG, slot: SLOT, expected: EXPECTED }],
            PAYER,
            U256::from(1_000u64),
            u64::MAX,
        )
    }

    #[test]
    fn revalidate_passes_when_unchanged_funded_and_unexpired() {
        let mut db = db_with(EXPECTED, 1_000);
        assert_eq!(manifest().revalidate(&mut db, 0), Ok(()));
    }

    #[test]
    fn revalidate_drops_on_config_slot_change() {
        let mut db = db_with(U256::from(43u64), 1_000);
        assert_eq!(
            manifest().revalidate(&mut db, 0),
            Err(ManifestStale::ConfigSlotChanged { address: CONFIG, slot: SLOT })
        );
    }

    #[test]
    fn revalidate_drops_on_payer_shortfall() {
        let mut db = db_with(EXPECTED, 999);
        assert_eq!(
            manifest().revalidate(&mut db, 0),
            Err(ManifestStale::PayerUnderfunded {
                payer: PAYER,
                required: U256::from(1_000u64),
                available: U256::from(999u64),
            })
        );
    }

    #[test]
    fn revalidate_drops_on_expiry() {
        let m = WatchManifest::new(vec![], PAYER, U256::ZERO, 100);
        let mut db = db_with(EXPECTED, 1_000);
        assert_eq!(
            m.revalidate(&mut db, 101),
            Err(ManifestStale::Expired { deadline: 100, now: 101 })
        );
        // Exactly at the deadline is still valid.
        assert_eq!(m.revalidate(&mut db, 100), Ok(()));
    }

    #[test]
    fn revalidate_fails_open_when_payer_account_absent() {
        // Payer never inserted: `basic` returns `Ok(None)`, which must not drop.
        let mut db = InMemoryDB::default();
        db.insert_account_storage(CONFIG, SLOT, EXPECTED).unwrap();
        assert_eq!(manifest().revalidate(&mut db, 0), Ok(()));
    }

    #[test]
    fn revalidate_skips_payer_check_when_cost_zero() {
        let m = WatchManifest::new(vec![], PAYER, U256::ZERO, u64::MAX);
        let mut db = InMemoryDB::default();
        assert_eq!(m.revalidate(&mut db, 0), Ok(()));
    }
}
