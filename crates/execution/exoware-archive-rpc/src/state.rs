//! reth [`StateProvider`] for the end of an archived block, used by the node's own EVM.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{Address, B256, BlockNumber, Bytes, StorageKey, StorageValue};
use base_exoware_archive::{AccountInfo, ArchiveError, ArchiveRequest, ExowareSession};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
    errors::provider::{ProviderError, ProviderResult},
};
use reth_trie_common::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    KeccakKeyHasher, MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof,
    StorageProof, TrieInput, updates::TrieUpdates,
};
use revm::database::BundleState;

use crate::{ArchiveExecutor, ArchiveHandle, ArchiveProvider};

/// The first archive failure of one execution, shared by its state provider and its
/// conclusion. The EVM and some inspectors turn a database error into a result or swallow
/// it, so the execution is judged by this record, not by what it returned.
#[derive(Debug, Clone, Default)]
pub struct ExecutionFailure {
    /// First failure, if any.
    pub first: Arc<Mutex<Option<ArchiveError>>>,
}

impl ExecutionFailure {
    /// Records `err` unless an earlier failure is recorded.
    pub fn record(&self, err: ArchiveError) {
        self.first.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(err);
    }

    /// The recorded failure, if any.
    pub fn get(&self) -> Option<ArchiveError> {
        self.first.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// State at the end of archived block `number`, read through one [`ArchiveRequest`] (so
/// every read sees the same segment set) until `deadline`. Accounts and slots come from the
/// archive; bytecode and block hashes, never pruned, from the node. Trie operations are not
/// served. Every failure is recorded in `failure` and reported to the EVM as
/// [`ProviderError::StateAtBlockPruned`].
pub struct ArchiveStateProvider<P> {
    /// Block whose post-state this is.
    pub number: u64,
    /// Archive reader.
    pub archive: Arc<ArchiveHandle<P>>,
    /// The request every read of the execution runs in.
    pub request: Arc<ArchiveRequest<ExowareSession>>,
    /// Node provider for bytecode and block hashes.
    pub provider: P,
    /// Runtime the reads run on.
    pub executor: Arc<ArchiveExecutor>,
    /// End of the whole execution's store budget.
    pub deadline: Instant,
    /// First failure of the execution.
    pub failure: ExecutionFailure,
}

impl<P> fmt::Debug for ArchiveStateProvider<P> {
    /// The block and deadline; the reader and provider are opaque.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveStateProvider")
            .field("number", &self.number)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl<P: ArchiveProvider> ArchiveStateProvider<P> {
    /// reth's account for an archived one: no code is `None`, as the node stores it.
    pub fn account(info: AccountInfo) -> Account {
        let code = info.code_hash != KECCAK_EMPTY && info.code_hash != B256::ZERO;
        Account {
            nonce: info.nonce,
            balance: info.balance,
            bytecode_hash: code.then_some(info.code_hash),
        }
    }

    /// Records `err` and returns the error the EVM sees.
    pub fn fail(&self, err: ArchiveError) -> ProviderError {
        self.failure.record(err);
        ProviderError::StateAtBlockPruned(self.number)
    }

    /// The error for a trie operation, which the archive does not serve.
    pub fn unsupported(&self, op: &str) -> ProviderError {
        self.fail(ArchiveError::Local {
            reason: format!("{op} is not served over archived state (block {})", self.number),
        })
    }

    /// Runs one archive read (`read` gets the reader handle and the execution's request) on
    /// the executor within the execution's deadline.
    pub fn read<T, Fut>(
        &self,
        read: impl FnOnce(Arc<ArchiveHandle<P>>, Arc<ArchiveRequest<ExowareSession>>) -> Fut,
    ) -> ProviderResult<T>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, ArchiveError>> + Send + 'static,
    {
        if let Some(err) = self.failure.get() {
            // The execution already failed; later reads are not worth a store call.
            return Err(self.fail(err));
        }
        let fut = read(Arc::clone(&self.archive), Arc::clone(&self.request));
        self.executor.run(fut, self.deadline).map_err(|err| self.fail(err))
    }
}

impl<P: ArchiveProvider> AccountReader for ArchiveStateProvider<P> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let (address, number) = (*address, self.number);
        self.read(move |archive, request| async move {
            archive.reader().await?.account_at_in(&request, address, number).await
        })
        .map(|info| info.map(Self::account))
    }
}

impl<P: ArchiveProvider> StateProvider for ArchiveStateProvider<P> {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let number = self.number;
        self.read(move |archive, request| async move {
            archive.reader().await?.storage_at_in(&request, account, storage_key, number).await
        })
        .map(Some)
    }
}

impl<P: ArchiveProvider> BytecodeReader for ArchiveStateProvider<P> {
    /// A non-empty code hash the node has no bytecode for is an error, never empty code.
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        if *code_hash == KECCAK_EMPTY {
            return Ok(None);
        }
        let code = self
            .provider
            .latest()
            .and_then(|state| state.bytecode_by_hash(code_hash))
            .map_err(|err| self.fail(ArchiveError::Local { reason: err.to_string() }))?;
        if code.is_none() {
            return Err(self.fail(ArchiveError::Local {
                reason: format!("bytecode {code_hash} is missing on the node"),
            }));
        }
        Ok(code)
    }
}

impl<P: ArchiveProvider> BlockHashReader for ArchiveStateProvider<P> {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.provider.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.provider.canonical_hashes_range(start, end)
    }
}

impl<P: ArchiveProvider> StateRootProvider for ArchiveStateProvider<P> {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        Err(self.unsupported("state_root"))
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Err(self.unsupported("state_root_from_nodes"))
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(self.unsupported("state_root_with_updates"))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(self.unsupported("state_root_from_nodes_with_updates"))
    }
}

impl<P: ArchiveProvider> StorageRootProvider for ArchiveStateProvider<P> {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Err(self.unsupported("storage_root"))
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(self.unsupported("storage_proof"))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(self.unsupported("storage_multiproof"))
    }
}

impl<P: ArchiveProvider> StateProofProvider for ArchiveStateProvider<P> {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(self.unsupported("proof"))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(self.unsupported("multiproof"))
    }

    fn multiproof_v2(
        &self,
        _targets: MultiProofTargetsV2,
        _requires_masks: bool,
    ) -> ProviderResult<DecodedMultiProofV2> {
        Err(self.unsupported("multiproof_v2"))
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        Err(self.unsupported("witness"))
    }
}

impl<P: ArchiveProvider> HashedPostStateProvider for ArchiveStateProvider<P> {
    /// The pure hashed transform of `bundle_state`. A destroyed account would also need its
    /// archived storage cleared, which the archive cannot enumerate, so that case is refused.
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        if bundle_state.state().values().any(|account| account.was_destroyed()) {
            return Err(self.unsupported("hashed_post_state of a destroyed account"));
        }
        Ok(HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first failure is kept; later ones do not replace it.
    #[test]
    fn execution_failure_keeps_the_first_error() {
        let failure = ExecutionFailure::default();
        assert_eq!(failure.get(), None, "a new execution has no failure");
        let shared = failure.clone();
        shared.record(ArchiveError::store("get", "first"));
        failure.record(ArchiveError::store("get", "second"));
        assert_eq!(
            failure.get(),
            Some(ArchiveError::store("get", "first")),
            "clones share one record and the first failure wins"
        );
    }

    /// Archive accounts map to the node's representation: no code is `None`.
    #[test]
    fn accounts_without_code_have_no_code_hash() {
        type Provider = ArchiveStateProvider<base_node_runner::BaseProvider>;
        let eoa = AccountInfo {
            nonce: 3,
            balance: alloy_primitives::U256::from(9),
            code_hash: KECCAK_EMPTY,
        };
        assert_eq!(
            Provider::account(eoa),
            Account { nonce: 3, balance: alloy_primitives::U256::from(9), bytecode_hash: None },
            "an EOA has no code hash"
        );
        let code = B256::repeat_byte(7);
        assert_eq!(
            Provider::account(AccountInfo { code_hash: code, ..eoa }).bytecode_hash,
            Some(code),
            "a contract keeps its code hash"
        );
    }
}
