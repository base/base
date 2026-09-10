//! Helper macros for implementing traits for various `StateProvider`
//! implementations

/// A macro that delegates trait implementations to the `as_ref` function of the type.
///
/// Used to implement provider traits.
#[macro_export]
macro_rules! delegate_impls_to_as_ref {
    (for $target:ty => $($trait:ident $(where [$($generics:tt)*])? {  $(fn $func:ident$(<$($generic_arg:ident: $generic_arg_ty:path),*>)?(&self, $($arg:ident: $argty:ty),*) -> $ret:path;)* })* ) => {

        $(
          impl<'a, $($($generics)*)?> $crate::$trait for $target {
              $(
                  fn $func$(<$($generic_arg: $generic_arg_ty),*>)?(&self, $($arg: $argty),*) -> $ret {
                    $crate::$trait::$func(&self.as_ref(), $($arg),*)
                  }
              )*
          }
        )*
    };
}

/// Delegates the provider trait implementations to the `as_ref` function of the type:
///
/// [`AccountReader`](crate::provider_api::AccountReader)
/// [`BlockHashReader`](crate::provider_api::BlockHashReader)
/// [`StateProvider`](crate::provider_api::StateProvider)
#[macro_export]
macro_rules! delegate_provider_impls {
    ($target:ty $(where [$($generics:tt)*])?) => {
        $crate::delegate_impls_to_as_ref!(
            for $target =>
            AccountReader $(where [$($generics)*])? {
                fn basic_account(&self, address: &alloy_primitives::Address) -> $crate::ProviderResult<Option<base_execution_evm_runtime::StoredAccount>>;
            }
            BlockHashReader $(where [$($generics)*])? {
                fn block_hash(&self, number: u64) -> $crate::ProviderResult<Option<alloy_primitives::B256>>;
                fn canonical_hashes_range(&self, start: alloy_primitives::BlockNumber, end: alloy_primitives::BlockNumber) -> $crate::ProviderResult<Vec<alloy_primitives::B256>>;
            }
            StateReadProvider $(where [$($generics)*])? {
                fn storage(&self, account: alloy_primitives::Address, storage_key: alloy_primitives::StorageKey) -> $crate::ProviderResult<Option<alloy_primitives::StorageValue>>;
            }
            BytecodeReader $(where [$($generics)*])? {
                fn bytecode_by_hash(&self, code_hash: &alloy_primitives::B256) -> $crate::ProviderResult<Option<base_execution_evm_runtime::StoredBytecode>>;
            }
            StateRootProvider $(where [$($generics)*])? {
                fn state_root(&self, state: base_execution_state_trie::HashedPostState) -> $crate::ProviderResult<alloy_primitives::B256>;
                fn state_root_from_nodes(&self, input: base_execution_state_trie::TrieInput) -> $crate::ProviderResult<alloy_primitives::B256>;
                fn state_root_with_updates(&self, state: base_execution_state_trie::HashedPostState) -> $crate::ProviderResult<(alloy_primitives::B256, base_execution_state_trie::updates::TrieUpdates)>;
                fn state_root_from_nodes_with_updates(&self, input: base_execution_state_trie::TrieInput) -> $crate::ProviderResult<(alloy_primitives::B256, base_execution_state_trie::updates::TrieUpdates)>;
            }
            StorageRootProvider $(where [$($generics)*])? {
                fn storage_root(&self, address: alloy_primitives::Address, storage: base_execution_state_trie::HashedStorage) -> $crate::ProviderResult<alloy_primitives::B256>;
                fn storage_proof(&self, address: alloy_primitives::Address, slot: alloy_primitives::B256, storage: base_execution_state_trie::HashedStorage) -> $crate::ProviderResult<base_execution_state_trie::StorageProof>;
                fn storage_multiproof(&self, address: alloy_primitives::Address, slots: &[alloy_primitives::B256], storage: base_execution_state_trie::HashedStorage) -> $crate::ProviderResult<base_execution_state_trie::StorageMultiProof>;
            }
            StateProofProvider $(where [$($generics)*])? {
                fn proof(&self, input: base_execution_state_trie::TrieInput, address: alloy_primitives::Address, slots: &[alloy_primitives::B256]) -> $crate::ProviderResult<base_execution_state_trie::AccountProof>;
                fn multiproof(&self, input: base_execution_state_trie::TrieInput, targets: base_execution_state_trie::MultiProofTargets) -> $crate::ProviderResult<base_execution_state_trie::MultiProof>;
                fn witness(&self, input: base_execution_state_trie::TrieInput, target: base_execution_state_trie::HashedPostState, mode: base_execution_state_trie::ExecutionWitnessMode) -> $crate::ProviderResult<Vec<alloy_primitives::Bytes>>;
            }
            HashedPostStateProvider $(where [$($generics)*])? {
                fn hashed_post_state(&self, bundle_state: &$crate::BundleState) -> $crate::ProviderResult<base_execution_state_trie::HashedPostState>;
            }
        );
        $crate::impl_state_database!([$($($generics)*)?] $target where []);
    }
}

/// Implements execution reads directly on a state provider.
///
/// Persisted account/code representations are converted at this boundary. Missing code,
/// storage, and block hashes retain the EVM's empty/zero semantics.
#[macro_export]
macro_rules! impl_state_database {
    ([$($generics:tt)*] $target:ty where [$($bounds:tt)*]) => {
        impl<$($generics)*> $crate::DatabaseRef for $target where $($bounds)* {
            type Error = $crate::ProviderError;

            fn basic_ref(&self, address: alloy_primitives::Address) -> Result<Option<$crate::AccountInfo>, Self::Error> {
                Ok($crate::AccountReader::basic_account(self, &address)?.map(Into::into))
            }

            fn code_by_hash_ref(&self, hash: alloy_primitives::B256) -> Result<$crate::Bytecode, Self::Error> {
                Ok($crate::BytecodeReader::bytecode_by_hash(self, &hash)?.unwrap_or_default().0)
            }

            fn storage_ref(&self, address: alloy_primitives::Address, key: alloy_primitives::U256) -> Result<alloy_primitives::U256, Self::Error> {
                Ok($crate::StateReadProvider::storage(self, address, alloy_primitives::B256::from(key))?.unwrap_or_default())
            }

            fn block_hash_ref(&self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
                Ok($crate::BlockHashReader::block_hash(self, number)?.unwrap_or_default())
            }
        }
        $crate::impl_read_only_database!([$($generics)*] $target where [$($bounds)*]);
        $crate::impl_read_only_database!(['__db, $($generics)*] &'__db $target where [$($bounds)*]);
    };
}

/// Exposes immutable execution reads through the mutable database interface.
#[macro_export]
macro_rules! impl_read_only_database {
    ([$($generics:tt)*] $target:ty where [$($bounds:tt)*]) => {
        impl<$($generics)*> $crate::Database for $target where $($bounds)* {
            type Error = <Self as $crate::DatabaseRef>::Error;

            fn basic(&mut self, address: alloy_primitives::Address) -> Result<Option<$crate::AccountInfo>, Self::Error> {
                $crate::DatabaseRef::basic_ref(self, address)
            }

            fn code_by_hash(&mut self, hash: alloy_primitives::B256) -> Result<$crate::Bytecode, Self::Error> {
                $crate::DatabaseRef::code_by_hash_ref(self, hash)
            }

            fn storage(&mut self, address: alloy_primitives::Address, key: alloy_primitives::U256) -> Result<alloy_primitives::U256, Self::Error> {
                $crate::DatabaseRef::storage_ref(self, address, key)
            }

            fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
                $crate::DatabaseRef::block_hash_ref(self, number)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use base_execution_evm_runtime::{
        Database, StoredAccount as Account, StoredBytecode as Bytecode,
    };
    use mockall::predicate::eq;

    use crate::{
        ProviderError, ProviderResult,
        provider_api::{AccountReader, BlockHashReader, BytecodeReader, StateReadProvider},
    };

    mockall::mock! {
        pub Reads {}
        impl AccountReader for Reads {
            fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>>;
        }
        impl BytecodeReader for Reads {
            fn bytecode_by_hash(&self, hash: &B256) -> ProviderResult<Option<Bytecode>>;
        }
        impl BlockHashReader for Reads {
            fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>>;
            fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>>;
        }
        impl StateReadProvider for Reads {
            fn storage(&self, address: Address, key: B256) -> ProviderResult<Option<U256>>;
        }
    }

    crate::impl_state_database!([] MockReads where []);

    #[test]
    fn execution_reads_do_not_require_proof_capabilities() {
        let address = Address::repeat_byte(1);
        let hash = B256::repeat_byte(2);
        let slot = U256::from(257);
        let account = Account { nonce: 7, balance: U256::from(100), bytecode_hash: Some(hash) };
        let mut reads = MockReads::new();
        reads.expect_basic_account().with(eq(address)).once().returning(move |_| Ok(Some(account)));
        reads.expect_bytecode_by_hash().with(eq(hash)).once().returning(|_| Ok(None));
        reads
            .expect_storage()
            .with(eq(address), eq(B256::from(slot)))
            .once()
            .returning(|_, _| Ok(None));
        reads.expect_block_hash().with(eq(10)).once().returning(|_| Ok(None));
        let mut db = reads;
        assert_eq!(db.basic(address).unwrap(), Some(account.into()));
        assert_eq!(db.code_by_hash(hash).unwrap(), base_execution_evm_runtime::Bytecode::default());
        assert_eq!(Database::storage(&mut db, address, slot).unwrap(), U256::ZERO);
        assert_eq!(Database::block_hash(&mut db, 10).unwrap(), B256::ZERO);
    }

    #[test]
    fn execution_reads_preserve_provider_errors() {
        let mut reads = MockReads::new();
        reads.expect_storage().once().returning(|_, _| Err(ProviderError::UnsupportedProvider));
        let mut db = reads;
        assert!(matches!(
            Database::storage(&mut db, Address::ZERO, U256::ZERO),
            Err(ProviderError::UnsupportedProvider)
        ));
    }
}
