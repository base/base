//! Captured authorization read-set for stateless intra-block revalidation.
//!
//! EIP-8130 validation snapshots every base-state storage slot read by the
//! authorizer, together with its value. A [`WatchManifest`] carries that exact
//! read-set plus payer-balance and expiry predicates to the payload builder,
//! where stale transactions can be dropped before execution without repeating
//! signature recovery.

use alloy_primitives::{Address, U256};
use revm::Database;

/// A storage slot read during EIP-8130 authorization and its expected value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSlot {
    /// Contract whose storage slot was read.
    pub address: Address,
    /// Storage slot key within `address`.
    pub slot: U256,
    /// Value the slot held when the transaction was authorized.
    pub expected: U256,
}

/// State predicates captured during EIP-8130 authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchManifest {
    config_slots: Vec<ConfigSlot>,
    payer: Address,
    payer_max_cost: U256,
    effective_expiry: u64,
}

impl Default for WatchManifest {
    fn default() -> Self {
        Self::new(Vec::new(), Address::ZERO, U256::ZERO, u64::MAX)
    }
}

impl WatchManifest {
    /// Builds a manifest from the captured authorization read-set and payer and
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

    /// Returns the storage slots and values read during authorization.
    #[must_use]
    pub fn config_slots(&self) -> &[ConfigSlot] {
        &self.config_slots
    }

    /// Returns the account funding the transaction.
    #[must_use]
    pub const fn payer(&self) -> Address {
        self.payer
    }

    /// Returns the maximum cost the transaction can charge the payer.
    #[must_use]
    pub const fn payer_max_cost(&self) -> U256 {
        self.payer_max_cost
    }

    /// Returns the last timestamp at which all captured expiry predicates hold.
    ///
    /// Transaction expiry is exclusive while actor expiry is inclusive, so this
    /// may be one less than the transaction's wire expiry. `u64::MAX` means
    /// unbounded.
    #[must_use]
    pub const fn effective_expiry(&self) -> u64 {
        self.effective_expiry
    }

    /// Returns whether authorization read no base-state storage slots.
    #[must_use]
    pub const fn has_no_config_slots(&self) -> bool {
        self.config_slots.is_empty()
    }

    /// Conservatively rechecks the captured predicates against build-time state.
    ///
    /// A positively observed slot change, payer shortfall, or expired deadline
    /// returns an error so the builder can drop the stale transaction before
    /// execution. Storage and account read errors fail open because execution
    /// remains authoritative and a transient read failure must not evict an
    /// otherwise-valid transaction. A successfully read absent payer is treated
    /// as an account with zero balance.
    ///
    /// `revm::Database` requires mutable access for reads. These reads may warm
    /// the EVM cache or add storage proofs in witness mode, but do not modify
    /// execution state.
    pub fn revalidate<DB: Database>(&self, db: &mut DB, now: u64) -> Result<(), ManifestStale> {
        if self.effective_expiry != u64::MAX && now > self.effective_expiry {
            return Err(ManifestStale::Expired { deadline: self.effective_expiry, now });
        }

        for slot in &self.config_slots {
            if let Ok(current) = db.storage(slot.address, slot.slot)
                && current != slot.expected
            {
                return Err(ManifestStale::ConfigSlotChanged {
                    address: slot.address,
                    slot: slot.slot,
                });
            }
        }

        if !self.payer_max_cost.is_zero()
            && let Ok(info) = db.basic(self.payer)
        {
            let available = info.map_or(U256::ZERO, |info| info.balance);
            if available < self.payer_max_cost {
                return Err(ManifestStale::PayerUnderfunded {
                    payer: self.payer,
                    required: self.payer_max_cost,
                    available,
                });
            }
        }

        Ok(())
    }
}

/// A positively observed reason an EIP-8130 transaction is stale at build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestStale {
    /// An authorization storage slot no longer has its expected value.
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
    /// The effective expiry deadline has passed.
    Expired {
        /// Effective expiry deadline in Unix seconds.
        deadline: u64,
        /// Build timestamp that passed the deadline.
        now: u64,
    },
}

impl ManifestStale {
    /// Returns a coarse, low-cardinality category suitable for a metric label.
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
    //! Manifest unit tests.
    //!
    //! [`FailingDatabase`] is hand-written because [`revm::Database`] is an external trait and
    //! every read must return the same custom associated error type; `automock` cannot be attached
    //! to that trait.

    use alloy_primitives::B256;
    use revm::{
        Database,
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        state::{AccountInfo, Bytecode},
    };

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
            U256::from(1_000),
            u64::MAX,
        )
    }

    #[test]
    fn default_manifest_is_inert() {
        let manifest = WatchManifest::default();
        let mut db = InMemoryDB::default();

        assert_eq!(manifest.effective_expiry(), u64::MAX);
        assert_eq!(manifest.revalidate(&mut db, u64::MAX), Ok(()));
    }

    #[test]
    fn revalidate_passes_when_unchanged_funded_and_unexpired() {
        let mut db = db_with(EXPECTED, 1_000);
        assert_eq!(manifest().revalidate(&mut db, 0), Ok(()));
    }

    #[test]
    fn revalidate_drops_on_config_slot_change() {
        let mut db = db_with(U256::from(43), 1_000);
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
                required: U256::from(1_000),
                available: U256::from(999),
            })
        );
    }

    #[test]
    fn revalidate_drops_only_after_expiry() {
        let manifest = WatchManifest::new(vec![], PAYER, U256::ZERO, 100);
        let mut db = db_with(EXPECTED, 1_000);
        assert_eq!(manifest.revalidate(&mut db, 100), Ok(()));
        assert_eq!(
            manifest.revalidate(&mut db, 101),
            Err(ManifestStale::Expired { deadline: 100, now: 101 })
        );
    }

    #[test]
    fn revalidate_treats_absent_payer_as_zero_balance() {
        let mut db = InMemoryDB::default();
        db.insert_account_storage(CONFIG, SLOT, EXPECTED).unwrap();
        assert_eq!(
            manifest().revalidate(&mut db, 0),
            Err(ManifestStale::PayerUnderfunded {
                payer: PAYER,
                required: U256::from(1_000),
                available: U256::ZERO,
            })
        );
    }

    #[test]
    fn revalidate_skips_zero_cost_payer_check() {
        let manifest = WatchManifest::new(vec![], PAYER, U256::ZERO, u64::MAX);
        assert_eq!(manifest.revalidate(&mut InMemoryDB::default(), 0), Ok(()));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("test database read failed")]
    struct ReadError;

    impl DBErrorMarker for ReadError {}

    #[derive(Debug, Default)]
    struct FailingDatabase;

    impl Database for FailingDatabase {
        type Error = ReadError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ReadError)
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Err(ReadError)
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(ReadError)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Err(ReadError)
        }
    }

    #[test]
    fn revalidate_fails_open_on_database_read_errors() {
        assert_eq!(manifest().revalidate(&mut FailingDatabase, 0), Ok(()));
    }
}
