use core::ops::{Deref, DerefMut};

use alloy_primitives::{Address, B256, U256};
use reth_storage_api::StateReadProvider;
use reth_storage_errors::provider::ProviderError;
use revm::{Database, DatabaseRef, bytecode::Bytecode, state::AccountInfo};

/// A [Database] and [`DatabaseRef`] implementation that uses [`StateReadProvider`] as the underlying
/// data source.
#[derive(Clone)]
pub struct StateProviderDatabase<DB>(pub DB);

impl<DB> StateProviderDatabase<DB> {
    /// Create new State with generic `StateReadProvider`.
    pub const fn new(db: DB) -> Self {
        Self(db)
    }

    /// Consume State and return inner `StateReadProvider`.
    pub fn into_inner(self) -> DB {
        self.0
    }
}

impl<DB> core::fmt::Debug for StateProviderDatabase<DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StateProviderDatabase").finish_non_exhaustive()
    }
}

impl<DB> AsRef<DB> for StateProviderDatabase<DB> {
    fn as_ref(&self) -> &DB {
        self
    }
}

impl<DB> Deref for StateProviderDatabase<DB> {
    type Target = DB;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<DB> DerefMut for StateProviderDatabase<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<DB: StateReadProvider> Database for StateProviderDatabase<DB> {
    type Error = ProviderError;

    /// Retrieves basic account information for a given address.
    ///
    /// Returns `Ok` with `Some(AccountInfo)` if the account exists,
    /// `None` if it doesn't, or an error if encountered.
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.basic_ref(address)
    }

    /// Retrieves the bytecode associated with a given code hash.
    ///
    /// Returns `Ok` with the bytecode if found, or the default bytecode otherwise.
    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.code_by_hash_ref(code_hash)
    }

    /// Retrieves the storage value at a specific index for a given address.
    ///
    /// Returns `Ok` with the storage value, or the default value if not found.
    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.storage_ref(address, index)
    }

    /// Retrieves the block hash for a given block number.
    ///
    /// Returns `Ok` with the block hash if found, or the default hash otherwise.
    /// Note: It safely casts the `number` to `u64`.
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.block_hash_ref(number)
    }
}

impl<DB: StateReadProvider> DatabaseRef for StateProviderDatabase<DB> {
    type Error = <Self as Database>::Error;

    /// Retrieves basic account information for a given address.
    ///
    /// Returns `Ok` with `Some(AccountInfo)` if the account exists,
    /// `None` if it doesn't, or an error if encountered.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.basic_account(&address)?.map(Into::into))
    }

    /// Retrieves the bytecode associated with a given code hash.
    ///
    /// Returns `Ok` with the bytecode if found, or the default bytecode otherwise.
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    /// Retrieves the storage value at a specific index for a given address.
    ///
    /// Returns `Ok` with the storage value, or the default value if not found.
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.0.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    /// Retrieves the block hash for a given block number.
    ///
    /// Returns `Ok` with the block hash if found, or the default hash otherwise.
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        // Get the block hash or default hash with an attempt to convert U256 block number to u64
        Ok(self.0.block_hash(number)?.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use mockall::predicate::eq;
    use reth_primitives_traits::{Account, Bytecode};
    use reth_storage_api::{AccountReader, BlockHashReader, BytecodeReader, StateReadProvider};
    use reth_storage_errors::provider::{ProviderError, ProviderResult};
    use revm::Database;

    use super::StateProviderDatabase;

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
        let mut db = StateProviderDatabase::new(reads);
        assert_eq!(db.basic(address).unwrap(), Some(account.into()));
        assert_eq!(db.code_by_hash(hash).unwrap(), revm::bytecode::Bytecode::default());
        assert_eq!(db.storage(address, slot).unwrap(), U256::ZERO);
        assert_eq!(db.block_hash(10).unwrap(), B256::ZERO);
    }

    #[test]
    fn execution_reads_preserve_provider_errors() {
        let mut reads = MockReads::new();
        reads.expect_storage().once().returning(|_, _| Err(ProviderError::UnsupportedProvider));
        let mut db = StateProviderDatabase::new(reads);
        assert!(matches!(
            db.storage(Address::ZERO, U256::ZERO),
            Err(ProviderError::UnsupportedProvider)
        ));
    }
}
