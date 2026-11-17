//! Captured admission predicates for stateless intra-block revalidation.
//!
//! A [`WatchManifest`] carries the payer-balance and expiry predicates of a
//! Base transaction admission to the payload builder, where stale transactions can be
//! dropped before execution without repeating signature recovery.

use alloy_primitives::{Address, U256};
use revm::Database;

use crate::UnixMillis;

/// State predicates captured during Base transaction admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchManifest {
    payer: Address,
    payer_max_cost: U256,
    effective_expiry: UnixMillis,
}

impl Default for WatchManifest {
    fn default() -> Self {
        Self::new(Address::ZERO, U256::ZERO, UnixMillis::MAX)
    }
}

impl WatchManifest {
    /// Builds a manifest from the captured payer and expiry predicates.
    #[must_use]
    pub const fn new(payer: Address, payer_max_cost: U256, effective_expiry: UnixMillis) -> Self {
        Self { payer, payer_max_cost, effective_expiry }
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

    /// Returns the last block time, in Unix milliseconds, at which all captured
    /// expiry predicates hold.
    ///
    /// This is the transaction's `TxBase::last_valid_ms`.
    /// [`UnixMillis::MAX`] means unbounded.
    #[must_use]
    pub const fn effective_expiry(&self) -> UnixMillis {
        self.effective_expiry
    }

    /// Conservatively rechecks the captured predicates against build-time state.
    ///
    /// `now` is the exact millisecond time of the block being built. A
    /// positively observed payer shortfall or expired deadline returns an error
    /// so the builder can drop the stale transaction before execution. Account
    /// read errors fail open because execution
    /// remains authoritative and a transient read failure must not evict an
    /// otherwise-valid transaction. A successfully read absent payer is treated
    /// as an account with zero balance.
    ///
    /// `revm::Database` requires mutable access for reads. These reads may warm
    /// the EVM cache or add storage proofs in witness mode, but do not modify
    /// execution state.
    pub fn revalidate<DB: Database>(
        &self,
        db: &mut DB,
        now: UnixMillis,
    ) -> Result<(), ManifestStale> {
        if now > self.effective_expiry {
            return Err(ManifestStale::Expired { deadline: self.effective_expiry, now });
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

/// A positively observed reason a Base transaction is stale at build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestStale {
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
        /// Last block time at which the transaction holds.
        deadline: UnixMillis,
        /// Time of the block being built.
        now: UnixMillis,
    },
}

impl ManifestStale {
    /// Returns a coarse, low-cardinality category suitable for a metric label.
    #[must_use]
    pub const fn cause(&self) -> &'static str {
        match self {
            Self::PayerUnderfunded { .. } => "payer_balance",
            Self::Expired { .. } => "expiry",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use revm::{
        Database,
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        state::{AccountInfo, Bytecode},
    };

    use super::*;

    const PAYER: Address = Address::repeat_byte(0x22);

    fn db_with(payer_balance: u64) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            PAYER,
            AccountInfo { balance: U256::from(payer_balance), ..Default::default() },
        );
        db
    }

    fn manifest() -> WatchManifest {
        WatchManifest::new(PAYER, U256::from(1_000), UnixMillis::MAX)
    }

    #[test]
    fn default_manifest_is_inert() {
        let manifest = WatchManifest::default();
        let mut db = InMemoryDB::default();

        assert_eq!(manifest.effective_expiry(), UnixMillis::MAX);
        assert_eq!(manifest.revalidate(&mut db, UnixMillis::MAX), Ok(()));
    }

    #[test]
    fn revalidate_passes_when_funded_and_unexpired() {
        let mut db = db_with(1_000);
        assert_eq!(manifest().revalidate(&mut db, UnixMillis::from_millis(0)), Ok(()));
    }

    #[test]
    fn revalidate_drops_on_payer_shortfall() {
        let mut db = db_with(999);
        assert_eq!(
            manifest().revalidate(&mut db, UnixMillis::from_millis(0)),
            Err(ManifestStale::PayerUnderfunded {
                payer: PAYER,
                required: U256::from(1_000),
                available: U256::from(999),
            })
        );
    }

    #[test]
    fn revalidate_drops_only_after_expiry() {
        // A sub-second deadline inside a header second shared by several blocks.
        let deadline = UnixMillis::from_millis(1_700_000_000_300);
        let manifest = WatchManifest::new(PAYER, U256::ZERO, deadline);
        let mut db = db_with(1_000);
        assert_eq!(manifest.revalidate(&mut db, deadline), Ok(()));
        let now = UnixMillis::from_millis(1_700_000_000_400);
        assert_eq!(
            manifest.revalidate(&mut db, now),
            Err(ManifestStale::Expired { deadline, now })
        );
    }

    #[test]
    fn revalidate_treats_absent_payer_as_zero_balance() {
        let mut db = InMemoryDB::default();
        assert_eq!(
            manifest().revalidate(&mut db, UnixMillis::from_millis(0)),
            Err(ManifestStale::PayerUnderfunded {
                payer: PAYER,
                required: U256::from(1_000),
                available: U256::ZERO,
            })
        );
    }

    #[test]
    fn revalidate_skips_zero_cost_payer_check() {
        let manifest = WatchManifest::new(PAYER, U256::ZERO, UnixMillis::MAX);
        assert_eq!(
            manifest.revalidate(&mut InMemoryDB::default(), UnixMillis::from_millis(0)),
            Ok(())
        );
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
        assert_eq!(manifest().revalidate(&mut FailingDatabase, UnixMillis::from_millis(0)), Ok(()));
    }
}
