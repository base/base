//! `BaseTime` predeploy artifacts, deployment, storage layout, and ABI surface.

use alloy_primitives::{Address, B256, Bytes, address, b256, hex};
use base_common_chains::Upgrades;
use base_common_consensus::Predeploys;
use revm::{
    DatabaseCommit,
    database_interface::Database,
    primitives::{HashMap, U256, uint},
    state::{Bytecode, EvmStorageSlot, TransactionId},
};

/// Read-only `BaseTime` predeploy data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseTime {
    /// Millisecond component written by the block-scoped metadata deposit.
    pub timestamp_millis_part: u16,
}

/// Errors produced while applying the `BaseTime` predeploy transition.
#[derive(Debug, thiserror::Error)]
pub enum BaseTimeTransitionError<DBError> {
    /// A database operation failed.
    #[error("BaseTime transition database error: {0}")]
    Database(#[from] DBError),
    /// The reserved proxy account is missing.
    #[error("BaseTime activation requires the reserved proxy account")]
    MissingProxy,
    /// The reserved proxy account does not contain code.
    #[error("BaseTime activation requires existing proxy code")]
    CodelessProxy,
    /// The reserved proxy does not have the canonical admin.
    #[error("BaseTime activation requires the canonical proxy admin, found {actual:#x}")]
    UnexpectedProxyAdmin {
        /// The admin slot value found in state.
        actual: U256,
    },
}

impl BaseTime {
    /// The code-namespace address used by the canonical `BaseTime` proxy.
    pub const IMPLEMENTATION_ADDRESS: Address =
        address!("0xc0D3C0d3C0d3C0D3c0d3C0d3c0D3C0d3c0d30030");

    /// The expected hash of the canonical `BaseTime` runtime bytecode.
    pub const IMPLEMENTATION_CODE_HASH: B256 =
        b256!("0x9c4c8a497a69d0b8f2ba67be0bee7a1373186055978c3be6ec3068e0ec27f32a");

    /// The expected hash of the canonical predeploy proxy runtime bytecode.
    pub const PROXY_CODE_HASH: B256 =
        b256!("0xfa8c9db6c6cab7108dea276f4cd09d575674eb0852c0fa3187e59e98ef977998");

    /// The EIP-1967 proxy implementation slot.
    pub const IMPLEMENTATION_SLOT: U256 =
        uint!(0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc_U256);

    /// The EIP-1967 proxy admin slot.
    pub const ADMIN_SLOT: U256 =
        uint!(0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103_U256);

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

    /// Returns the canonical `BaseTime` runtime bytecode from `base/contracts` commit `4848ec70`.
    pub fn implementation_bytecode() -> Bytes {
        hex::decode(include_str!("bytecode/base-time.hex").trim())
            .expect("BaseTime runtime artifact must be valid hex")
            .into()
    }

    /// Returns the canonical predeploy proxy runtime from `base/contracts` commit `4848ec70`.
    pub fn proxy_bytecode() -> Bytes {
        hex::decode(include_str!("bytecode/proxy.hex").trim())
            .expect("proxy runtime artifact must be valid hex")
            .into()
    }

    /// Installs the `BaseTime` implementation and links its existing proxy before transactions.
    ///
    /// Existing chains must already contain the reserved proxy runtime with
    /// [`Predeploys::PROXY_ADMIN`] in its EIP-1967 admin slot. This transition validates that
    /// historical invariant; it does not install or repair proxy state.
    ///
    /// The transition is staged behind the `Zombie` activation condition. The implementation slot
    /// is the durable migration marker: any existing linkage is preserved so later execution cannot
    /// rewrite the initial deployment or undo a governance upgrade.
    pub fn ensure_predeploy<DB>(
        chain_spec: impl Upgrades,
        timestamp: u64,
        db: &mut DB,
    ) -> Result<(), BaseTimeTransitionError<DB::Error>>
    where
        DB: Database + DatabaseCommit,
    {
        if !chain_spec.is_zombie_active_at_timestamp(timestamp) {
            return Ok(());
        }

        let expected_implementation = U256::from_be_slice(Self::IMPLEMENTATION_ADDRESS.as_slice());
        let current_implementation =
            db.storage(Predeploys::BASE_TIME, Self::IMPLEMENTATION_SLOT)?;
        if current_implementation != U256::ZERO {
            return Ok(());
        }

        let proxy_info =
            db.basic(Predeploys::BASE_TIME)?.ok_or(BaseTimeTransitionError::MissingProxy)?;
        if proxy_info.is_empty_code_hash() {
            return Err(BaseTimeTransitionError::CodelessProxy);
        }

        let current_admin = db.storage(Predeploys::BASE_TIME, Self::ADMIN_SLOT)?;
        if current_admin != U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()) {
            return Err(BaseTimeTransitionError::UnexpectedProxyAdmin { actual: current_admin });
        }

        let code = Bytecode::new_raw(Self::implementation_bytecode());
        let mut implementation_info = db.basic(Self::IMPLEMENTATION_ADDRESS)?.unwrap_or_default();
        implementation_info.code_hash = code.hash_slow();
        implementation_info.code = Some(code);

        let mut implementation_account: revm::state::Account = implementation_info.into();
        implementation_account.mark_touch();

        let mut proxy_account: revm::state::Account = proxy_info.into();
        proxy_account.storage.insert(
            Self::IMPLEMENTATION_SLOT,
            EvmStorageSlot::new_changed(
                current_implementation,
                expected_implementation,
                TransactionId::ZERO,
            ),
        );
        proxy_account.mark_touch();

        db.commit(HashMap::from_iter([
            (Self::IMPLEMENTATION_ADDRESS, implementation_account),
            (Predeploys::BASE_TIME, proxy_account),
        ]));
        Ok(())
    }

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
    use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
    use alloy_primitives::{address, keccak256};
    use base_common_genesis::BaseUpgrade;
    use revm::{Database as _, database::InMemoryDB, state::AccountInfo};

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

    #[test]
    fn artifacts_match_canonical_contracts() {
        assert_eq!(
            keccak256(BaseTime::implementation_bytecode()),
            BaseTime::IMPLEMENTATION_CODE_HASH
        );
        assert_eq!(keccak256(BaseTime::proxy_bytecode()), BaseTime::PROXY_CODE_HASH);
    }

    #[test]
    fn active_transition_installs_and_links_implementation() {
        let mut db = base_time_db(600);
        let proxy_code = Bytecode::new_raw(BaseTime::proxy_bytecode());
        db.insert_account_info(
            Predeploys::BASE_TIME,
            AccountInfo {
                code_hash: proxy_code.hash_slow(),
                code: Some(proxy_code),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            Predeploys::BASE_TIME,
            BaseTime::ADMIN_SLOT,
            U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
        )
        .unwrap();

        BaseTime::ensure_predeploy(TestUpgrades(true), 100, &mut db).unwrap();

        let proxy = db.basic(Predeploys::BASE_TIME).unwrap().unwrap();
        assert_eq!(proxy.code.unwrap().original_bytes(), BaseTime::proxy_bytecode());
        assert_eq!(
            db.storage(Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT).unwrap(),
            U256::from(600)
        );
        assert_eq!(
            db.storage(Predeploys::BASE_TIME, BaseTime::ADMIN_SLOT).unwrap(),
            U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice())
        );
        assert_eq!(
            db.storage(Predeploys::BASE_TIME, BaseTime::IMPLEMENTATION_SLOT).unwrap(),
            U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice())
        );

        let implementation = db.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().unwrap();
        assert_eq!(implementation.code_hash, BaseTime::IMPLEMENTATION_CODE_HASH);

        BaseTime::ensure_predeploy(TestUpgrades(true), 100, &mut db).unwrap();
        assert_eq!(
            db.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().unwrap().code_hash,
            BaseTime::IMPLEMENTATION_CODE_HASH
        );
    }

    #[test]
    fn inactive_transition_is_noop() {
        let mut db = InMemoryDB::default();

        BaseTime::ensure_predeploy(TestUpgrades(false), 100, &mut db).unwrap();

        assert!(db.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().is_none());
    }

    #[test]
    fn active_transition_rejects_missing_proxy() {
        let mut db = InMemoryDB::default();

        let error = BaseTime::ensure_predeploy(TestUpgrades(true), 102, &mut db).unwrap_err();

        assert!(matches!(error, BaseTimeTransitionError::MissingProxy));
    }

    #[test]
    fn active_transition_rejects_codeless_proxy() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(Predeploys::BASE_TIME, AccountInfo::default());

        let error = BaseTime::ensure_predeploy(TestUpgrades(true), 102, &mut db).unwrap_err();

        assert!(matches!(error, BaseTimeTransitionError::CodelessProxy));
    }

    #[test]
    fn active_transition_rejects_unexpected_proxy_admin() {
        let mut db = InMemoryDB::default();
        let proxy_code = Bytecode::new_raw(BaseTime::proxy_bytecode());
        db.insert_account_info(
            Predeploys::BASE_TIME,
            AccountInfo {
                code_hash: proxy_code.hash_slow(),
                code: Some(proxy_code),
                ..Default::default()
            },
        );

        let error = BaseTime::ensure_predeploy(TestUpgrades(true), 102, &mut db).unwrap_err();

        assert!(matches!(
            error,
            BaseTimeTransitionError::UnexpectedProxyAdmin { actual: U256::ZERO }
        ));
    }

    #[test]
    fn transition_preserves_linked_canonical_implementation() {
        let mut db = InMemoryDB::default();
        let existing_code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        let existing_code_hash = existing_code.hash_slow();
        db.insert_account_info(
            BaseTime::IMPLEMENTATION_ADDRESS,
            AccountInfo {
                code_hash: existing_code_hash,
                code: Some(existing_code),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            Predeploys::BASE_TIME,
            BaseTime::IMPLEMENTATION_SLOT,
            U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice()),
        )
        .unwrap();

        BaseTime::ensure_predeploy(TestUpgrades(true), 100, &mut db).unwrap();

        assert_eq!(
            db.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().unwrap().code_hash,
            existing_code_hash
        );
    }

    #[test]
    fn transition_preserves_existing_proxy_upgrade() {
        let mut db = InMemoryDB::default();
        let other_implementation = address!("0x1111111111111111111111111111111111111111");
        db.insert_account_storage(
            Predeploys::BASE_TIME,
            BaseTime::IMPLEMENTATION_SLOT,
            U256::from_be_slice(other_implementation.as_slice()),
        )
        .unwrap();

        BaseTime::ensure_predeploy(TestUpgrades(true), 100, &mut db).unwrap();

        assert_eq!(
            db.storage(Predeploys::BASE_TIME, BaseTime::IMPLEMENTATION_SLOT).unwrap(),
            U256::from_be_slice(other_implementation.as_slice())
        );
        assert!(db.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().is_none());
    }

    #[derive(Clone, Copy)]
    struct TestUpgrades(bool);

    impl EthereumHardforks for TestUpgrades {
        fn ethereum_fork_activation(&self, _fork: EthereumHardfork) -> ForkCondition {
            ForkCondition::Never
        }
    }

    impl Upgrades for TestUpgrades {
        fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
            if fork == BaseUpgrade::Zombie && self.0 {
                ForkCondition::Timestamp(100)
            } else {
                ForkCondition::Never
            }
        }
    }
}
