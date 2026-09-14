//! Withdrawals-root calculation for locally built post-Isthmus payloads.

use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use base_common_chains::Upgrades;
use base_common_consensus::Predeploys;
use reth_storage_api::{StorageRootProvider, errors::ProviderResult};
use revm::database::BundleState;

use crate::isthmus;

/// Computes the post-Isthmus withdrawals root for locally built payloads.
#[derive(Debug)]
pub struct WithdrawalsRoot;

impl WithdrawalsRoot {
    /// Reuses the parent header's root when `MessagePasser` storage is unchanged.
    ///
    /// `state_updates` must contain the cumulative changes relative to `parent`, including
    /// earlier flashblocks, and `state` must be a provider for that same parent. At the
    /// Isthmus boundary, the parent field still has its old meaning and cannot be reused.
    /// Callers must only use this helper for post-Isthmus payloads.
    pub fn compute<DB: StorageRootProvider>(
        state_updates: &BundleState,
        state: DB,
        parent: &impl BlockHeader,
        chain_spec: &impl Upgrades,
    ) -> ProviderResult<B256> {
        if chain_spec.is_isthmus_active_at_timestamp(parent.timestamp())
            && let Some(root) = parent.withdrawals_root()
            && state_updates.state().get(&Predeploys::L2_TO_L1_MESSAGE_PASSER).is_none_or(
                |account| {
                    !account.was_destroyed()
                        && account.storage.values().all(|slot| !slot.is_changed())
                },
            )
        {
            return Ok(root);
        }

        isthmus::withdrawals_root(state_updates, state)
    }
}

#[cfg(test)]
pub mod tests {
    //! Test fixtures and regression tests for parent withdrawals-root reuse.

    use alloy_consensus::Header;
    use alloy_primitives::{Address, U256, keccak256};
    use base_common_genesis::BaseUpgrade;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use mockall::mock;
    use reth_chainspec::ForkCondition;
    use reth_storage_api::errors::ProviderError;
    use reth_trie::test_utils::storage_root_prehashed;
    use reth_trie_common::{HashedStorage, StorageMultiProof, StorageProof};
    use revm::{
        database::{AccountStatus, BundleAccount, states::StorageSlot},
        state::AccountInfo,
    };

    use super::*;

    mock! {
        pub StorageProvider {}

        impl StorageRootProvider for StorageProvider {
            fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256>;
            fn storage_proof(
                &self,
                address: Address,
                slot: B256,
                storage: HashedStorage,
            ) -> ProviderResult<StorageProof>;
            fn storage_multiproof(
                &self,
                address: Address,
                slots: &[B256],
                storage: HashedStorage,
            ) -> ProviderResult<StorageMultiProof>;
        }
    }

    /// Parent state and execution updates for withdrawals-root tests.
    #[derive(Debug)]
    pub struct Fixture {
        /// Chain configuration with Isthmus activated at timestamp 100.
        pub chain_spec: BaseChainSpec,
        /// Parent header committing to the original `MessagePasser` storage.
        pub parent: Header,
        /// Cumulative candidate changes relative to the parent.
        pub updates: BundleState,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Self {
                chain_spec: BaseChainSpecBuilder::base_mainnet()
                    .with_fork(BaseUpgrade::Isthmus, ForkCondition::Timestamp(100))
                    .build(),
                parent: Header {
                    timestamp: 100,
                    withdrawals_root: Some(storage_root_prehashed([(
                        keccak256(U256::from(1).to_be_bytes::<32>()),
                        U256::from(1),
                    )])),
                    ..Default::default()
                },
                updates: BundleState::default(),
            }
        }
    }

    impl Fixture {
        /// Sets a `MessagePasser` account update with one storage slot and a balance change.
        pub fn set_account(&mut self, status: AccountStatus, original: u64, present: u64) {
            self.updates.state.insert(
                Predeploys::L2_TO_L1_MESSAGE_PASSER,
                BundleAccount::new(
                    Some(AccountInfo::default()),
                    Some(AccountInfo { balance: U256::from(10), ..Default::default() }),
                    [(
                        U256::from(1),
                        StorageSlot::new_changed(U256::from(original), U256::from(present)),
                    )]
                    .into_iter()
                    .collect(),
                    status,
                ),
            );
        }
    }

    #[test]
    fn unchanged_storage_skips_provider() {
        let mut fixture = Fixture::default();
        let provider = MockStorageProvider::new(); // Any provider call fails the test.
        for status in [None, Some(AccountStatus::Loaded), Some(AccountStatus::Changed)] {
            if let Some(status) = status {
                fixture.set_account(status, 1, 1);
            }
            assert_eq!(
                WithdrawalsRoot::compute(
                    &fixture.updates,
                    &provider,
                    &fixture.parent,
                    &fixture.chain_spec,
                )
                .unwrap(),
                fixture.parent.withdrawals_root.unwrap(),
            );
        }
    }

    #[test]
    fn writes_restored_to_original_value_reuse_parent() {
        let mut fixture = Fixture::default();
        fixture.set_account(AccountStatus::Changed, 1, 2);
        fixture
            .updates
            .state
            .get_mut(&Predeploys::L2_TO_L1_MESSAGE_PASSER)
            .unwrap()
            .storage
            .get_mut(&U256::from(1))
            .unwrap()
            .present_value = U256::from(1);
        assert_eq!(
            WithdrawalsRoot::compute(
                &fixture.updates,
                MockStorageProvider::new(),
                &fixture.parent,
                &fixture.chain_spec,
            )
            .unwrap(),
            fixture.parent.withdrawals_root.unwrap(),
        );
    }

    #[test]
    fn cumulative_storage_changes_match_existing_calculation() {
        let mut fixture = Fixture::default();
        let mut provider = MockStorageProvider::new();
        provider.expect_storage_root().times(6).returning(|address, storage| {
            assert_eq!(address, Predeploys::L2_TO_L1_MESSAGE_PASSER);
            Ok(storage_root_prehashed(storage.storage))
        });

        // An initial withdrawal, no new writes in the next flashblock, then another withdrawal.
        // Original values remain relative to the parent, not the previous flashblock.
        for present in [2, 2, 3] {
            fixture.set_account(AccountStatus::Changed, 1, present);
            let expected = isthmus::withdrawals_root(&fixture.updates, &provider).unwrap();
            assert_ne!(Some(expected), fixture.parent.withdrawals_root);
            assert_eq!(
                WithdrawalsRoot::compute(
                    &fixture.updates,
                    &provider,
                    &fixture.parent,
                    &fixture.chain_spec,
                )
                .unwrap(),
                expected,
            );
        }
    }

    #[test]
    fn storage_destruction_does_not_reuse_parent() {
        let mut fixture = Fixture::default();
        let mut provider = MockStorageProvider::new();
        provider.expect_storage_root().times(3).return_const(Ok(B256::ZERO));
        for status in [
            AccountStatus::Destroyed,
            AccountStatus::DestroyedChanged,
            AccountStatus::DestroyedAgain,
        ] {
            fixture.set_account(status, 1, 1);
            fixture
                .updates
                .state
                .get_mut(&Predeploys::L2_TO_L1_MESSAGE_PASSER)
                .unwrap()
                .storage
                .clear();
            assert_eq!(
                WithdrawalsRoot::compute(
                    &fixture.updates,
                    &provider,
                    &fixture.parent,
                    &fixture.chain_spec,
                )
                .unwrap(),
                B256::ZERO,
            );
        }
    }

    #[test]
    fn activation_boundary_and_missing_root_fall_back() {
        let mut fixture = Fixture::default();
        let mut provider = MockStorageProvider::new();
        provider.expect_storage_root().times(2).return_const(Ok(B256::ZERO));
        for (timestamp, root) in [(99, Some(alloy_trie::EMPTY_ROOT_HASH)), (100, None)] {
            fixture.parent.timestamp = timestamp;
            fixture.parent.withdrawals_root = root;
            assert_eq!(
                WithdrawalsRoot::compute(
                    &fixture.updates,
                    &provider,
                    &fixture.parent,
                    &fixture.chain_spec,
                )
                .unwrap(),
                B256::ZERO,
            );
        }
    }

    #[test]
    fn reuse_follows_actual_parent() {
        let mut fixture = Fixture::default();
        let provider = MockStorageProvider::new();
        for root in [B256::repeat_byte(1), B256::repeat_byte(2)] {
            fixture.parent.withdrawals_root = Some(root);
            assert_eq!(
                WithdrawalsRoot::compute(
                    &fixture.updates,
                    &provider,
                    &fixture.parent,
                    &fixture.chain_spec,
                )
                .unwrap(),
                root,
            );
        }
    }

    #[test]
    fn computation_errors_propagate() {
        let mut fixture = Fixture::default();
        fixture.set_account(AccountStatus::Changed, 1, 2);
        let mut provider = MockStorageProvider::new();
        provider
            .expect_storage_root()
            .once()
            .returning(|_, _| Err(ProviderError::UnsupportedProvider));
        assert!(matches!(
            WithdrawalsRoot::compute(
                &fixture.updates,
                &provider,
                &fixture.parent,
                &fixture.chain_spec,
            ),
            Err(ProviderError::UnsupportedProvider),
        ));
    }
}
