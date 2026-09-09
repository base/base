use alloy_primitives::{Address, B256, U256};
use base_common_types_chain::BaseReceipt;
pub use base_evm_handler::BlockExecutionResult;
use base_evm_handler::database::{BundleAccount, BundleState};
use reth_primitives_traits::{Account, Bytecode};

/// [`BlockExecutionResult`] combined with state.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    derive_more::AsRef,
    derive_more::AsMut,
    derive_more::Deref,
    derive_more::DerefMut,
)]
pub struct BlockExecutionOutput {
    /// All the receipts of the transactions in the block.
    #[as_ref]
    #[as_mut]
    #[deref]
    #[deref_mut]
    pub result: BlockExecutionResult<BaseReceipt>,
    /// The changed state of the block after execution.
    pub state: BundleState,
}

impl BlockExecutionOutput {
    /// Return bytecode if known.
    pub fn bytecode(&self, code_hash: &B256) -> Option<Bytecode> {
        self.state.bytecode(code_hash).map(Bytecode)
    }

    /// Get account if account is known.
    pub fn account(&self, address: &Address) -> Option<Option<Account>> {
        self.state.account(address).map(|a| a.info.as_ref().map(Into::into))
    }

    /// Returns the state [`BundleAccount`] for the given address.
    pub fn account_state(&self, address: &Address) -> Option<&BundleAccount> {
        self.state.account(address)
    }

    /// Get storage if value is known.
    ///
    /// This means that depending on status we can potentially return `U256::ZERO`.
    pub fn storage(&self, address: &Address, storage_key: U256) -> Option<U256> {
        self.state.account(address).and_then(|a| a.storage_slot(storage_key))
    }
}

impl Default for BlockExecutionOutput {
    fn default() -> Self {
        Self {
            result: BlockExecutionResult {
                receipts: Default::default(),
                requests: Default::default(),
                gas_used: 0,
                blob_gas_used: 0,
            },
            state: Default::default(),
        }
    }
}
