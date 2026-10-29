//! A typed [`Database`] view of a builder's accepted EVM state.

use alloy_primitives::{Address, B256};
use evm2::{
    DatabaseError,
    bytecode::Bytecode,
    evm::{AccountInfo, Database, DynDatabase},
    interpreter::Word,
};

/// Exposes a [`DynDatabase`], such as the overlay a block builder's EVM reads its accepted state
/// through, to code generic over [`Database`].
#[derive(Debug)]
pub struct StateView<'a, DB: ?Sized>(pub &'a mut DB);

impl<DB: DynDatabase + ?Sized> Database for StateView<'_, DB> {
    type Error = DatabaseError;

    fn is_fatal(error: &Self::Error) -> bool {
        error.is_fatal()
    }

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.get_account(address)
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
        self.0.get_code_by_hash(code_hash)
    }

    fn get_storage(&mut self, address: &Address, key: &Word) -> Result<Word, Self::Error> {
        self.0.get_storage(address, key)
    }

    fn get_block_hash(&mut self, number: &Word) -> Result<B256, Self::Error> {
        self.0.get_block_hash(number)
    }
}
