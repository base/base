//! Read-only view over the `BaseTime` predeploy's storage layout and ABI surface.

use alloy_primitives::hex;
use base_common_consensus::Predeploys;
use revm::{
    database_interface::Database,
    primitives::{U256, uint},
};

/// Read-only `BaseTime` predeploy data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseTime {
    /// Millisecond component written by the block-scoped metadata deposit.
    pub timestamp_millis_part: u16,
}

impl BaseTime {
    /// The selector of `setTimestampMillisPart(uint16)`.
    pub const SET_TIMESTAMP_MILLIS_PART_SELECTOR: [u8; 4] = hex!("86bdf394");

    /// The selector of `timestampMillisPart()`.
    pub const TIMESTAMP_MILLIS_PART_SELECTOR: [u8; 4] = hex!("7b2fea99");

    /// The selector of `timestampMs()`.
    pub const TIMESTAMP_MS_SELECTOR: [u8; 4] = hex!("5745a677");

    /// The slot that stores the `timestamp_millis_part` value.
    pub const TIMESTAMP_MILLIS_PART_SLOT: U256 = uint!(0_U256);

    /// Byte offset within the 32-byte storage slot of the packed `uint16` value.
    pub const TIMESTAMP_MILLIS_PART_OFFSET: usize = 30;

    /// Fetches the `timestamp_millis_part` directly from the `BaseTime` predeploy storage.
    pub fn fetch_timestamp_millis_part<DB: Database>(db: &mut DB) -> Result<u16, DB::Error> {
        let slot = db
            .storage(Predeploys::BASE_TIME, Self::TIMESTAMP_MILLIS_PART_SLOT)?
            .to_be_bytes::<32>();

        Ok(u16::from_be_bytes([
            slot[Self::TIMESTAMP_MILLIS_PART_OFFSET],
            slot[Self::TIMESTAMP_MILLIS_PART_OFFSET + 1],
        ]))
    }

    /// Loads the current `BaseTime` state from the database.
    pub fn try_fetch<DB: Database>(db: &mut DB) -> Result<Self, DB::Error> {
        let _ = db.basic(Predeploys::BASE_TIME)?;

        Ok(Self { timestamp_millis_part: Self::fetch_timestamp_millis_part(db)? })
    }

    /// Computes the full millisecond timestamp using the canonical block timestamp in seconds.
    pub fn timestamp_ms(&self, block_timestamp: u64) -> u64 {
        block_timestamp.wrapping_mul(1_000).wrapping_add(u64::from(self.timestamp_millis_part))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;
    use revm::{database::InMemoryDB, state::AccountInfo};

    use super::*;

    fn base_time_db(value: u16) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(Predeploys::BASE_TIME, AccountInfo::default());
        db.insert_account_storage(
            Predeploys::BASE_TIME,
            BaseTime::TIMESTAMP_MILLIS_PART_SLOT,
            U256::from(value),
        )
        .expect("base time slot should be writable");
        db
    }

    #[test]
    fn fetch_timestamp_millis_part_reads_slot_zero() {
        let mut db = base_time_db(600);

        assert_eq!(BaseTime::fetch_timestamp_millis_part(&mut db).unwrap(), 600);
    }

    #[test]
    fn try_fetch_returns_zero_for_unset_storage() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(Predeploys::BASE_TIME, AccountInfo::default());

        assert_eq!(BaseTime::try_fetch(&mut db).unwrap(), BaseTime::default());
    }

    #[test]
    fn timestamp_ms_adds_seconds_and_millis_part() {
        let base_time = BaseTime { timestamp_millis_part: 800 };

        assert_eq!(base_time.timestamp_ms(1_725), 1_725_800);
    }

    #[test]
    fn timestamp_ms_matches_solidity_uint64_truncation() {
        let base_time = BaseTime { timestamp_millis_part: 800 };
        let block_timestamp = u64::MAX / 1_000 + 1;

        assert_eq!(
            base_time.timestamp_ms(block_timestamp),
            block_timestamp.wrapping_mul(1_000).wrapping_add(800)
        );
    }

    #[test]
    fn selectors_match_solidity_signatures() {
        let set_selector: [u8; 4] =
            keccak256("setTimestampMillisPart(uint16)")[..4].try_into().unwrap();
        let millis_part_selector: [u8; 4] =
            keccak256("timestampMillisPart()")[..4].try_into().unwrap();
        let timestamp_ms_selector: [u8; 4] = keccak256("timestampMs()")[..4].try_into().unwrap();

        assert_eq!(BaseTime::SET_TIMESTAMP_MILLIS_PART_SELECTOR, set_selector);
        assert_eq!(BaseTime::TIMESTAMP_MILLIS_PART_SELECTOR, millis_part_selector);
        assert_eq!(BaseTime::TIMESTAMP_MS_SELECTOR, timestamp_ms_selector);
    }
}
