//! EIP-8130 execution metadata carried alongside the REVM transaction.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, U256};
use base_common_consensus::{
    ACCOUNT_CONFIG_ADDRESS, AccountChangeEntry, ConfigChangeEntry, CreateEntry, TxEip8130,
    auto_delegation_code, config_change_sequence, config_change_writes, derive_account_address,
    implicit_eoa_owner_id, owner_registration_writes,
};

/// A storage write produced by EIP-8130 account changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eip8130StorageWrite {
    /// Contract address holding the storage.
    pub address: Address,
    /// Storage slot key.
    pub slot: U256,
    /// New value to write.
    pub value: U256,
}

/// A code placement produced by EIP-8130 account creation or delegation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eip8130CodePlacement {
    /// Address receiving code.
    pub address: Address,
    /// Bytecode to install.
    pub code: Bytes,
}

/// A packed AccountConfiguration sequence update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eip8130SequenceUpdate {
    /// Storage slot for `_accountState[account]`.
    pub slot: U256,
    /// Whether this updates the multichain sequence field.
    pub is_multichain: bool,
    /// New sequence value.
    pub new_value: u64,
}

/// A user call in an EIP-8130 execution phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eip8130Call {
    /// Target address.
    pub to: Address,
    /// Calldata for the target.
    pub data: Bytes,
    /// ETH value for the call.
    pub value: U256,
}

/// EIP-8130 execution metadata derived from the transaction envelope.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Eip8130Parts {
    /// Sender account.
    pub sender: Address,
    /// Gas payer account.
    pub payer: Address,
    /// Authenticated owner id for TxContext.
    pub owner_id: B256,
    /// Nonce lane key.
    pub nonce_key: U256,
    /// Optional delegation target from account changes.
    pub delegation_target: Option<Address>,
    /// EIP-7702-style default account delegation code.
    pub auto_delegation_code: Bytes,
    /// Writes applied before user calls, such as initial owner registrations.
    pub pre_writes: Vec<Eip8130StorageWrite>,
    /// Owner config writes applied after validation.
    pub config_writes: Vec<Eip8130StorageWrite>,
    /// Packed sequence updates for config changes.
    pub sequence_updates: Vec<Eip8130SequenceUpdate>,
    /// Account code placements from create entries.
    pub code_placements: Vec<Eip8130CodePlacement>,
    /// Phased user calls.
    pub call_phases: Vec<Vec<Eip8130Call>>,
}

impl Eip8130Parts {
    /// Builds execution metadata from a decoded EIP-8130 transaction.
    pub fn from_tx(tx: &TxEip8130, recovered_caller: Address) -> Self {
        let mut pre_writes = Vec::new();
        let mut config_writes = Vec::new();
        let mut sequence_updates = Vec::new();
        let mut code_placements = Vec::new();
        let mut delegation_target = None;

        for entry in &tx.account_changes {
            match entry {
                AccountChangeEntry::Create(create) => {
                    let account = account_address(create);
                    pre_writes.extend(owner_registration_writes(account, create).into_iter().map(
                        |write| Eip8130StorageWrite {
                            address: write.address,
                            slot: write.slot,
                            value: write.value,
                        },
                    ));
                    code_placements.push(Eip8130CodePlacement {
                        address: account,
                        code: create_bytecode(create),
                    });
                }
                AccountChangeEntry::ConfigChange(change) => {
                    config_writes.extend(
                        config_change_writes(recovered_caller, change).into_iter().map(|write| {
                            Eip8130StorageWrite {
                                address: write.address,
                                slot: write.slot,
                                value: write.value,
                            }
                        }),
                    );
                    sequence_updates.push(sequence_update(recovered_caller, change));
                }
                AccountChangeEntry::Delegation(delegation) => {
                    delegation_target = Some(delegation.target);
                }
            }
        }

        let call_phases = tx
            .calls
            .iter()
            .map(|phase| {
                phase
                    .iter()
                    .map(|call| Eip8130Call {
                        to: call.to,
                        data: call.data.clone(),
                        value: U256::ZERO,
                    })
                    .collect()
            })
            .collect();

        Self {
            sender: recovered_caller,
            payer: tx.payer.unwrap_or(recovered_caller),
            owner_id: implicit_eoa_owner_id(recovered_caller),
            nonce_key: tx.nonce_key,
            delegation_target,
            auto_delegation_code: auto_delegation_code(),
            pre_writes,
            config_writes,
            sequence_updates,
            code_placements,
            call_phases,
        }
    }
}

/// Derives the account address for a create entry.
pub fn account_address(create: &CreateEntry) -> Address {
    derive_account_address(
        ACCOUNT_CONFIG_ADDRESS,
        create.user_salt,
        &create.bytecode,
        &create.initial_owners,
    )
}

/// Returns the code that should be installed for a create entry.
pub fn create_bytecode(create: &CreateEntry) -> Bytes {
    if create.bytecode.is_empty() {
        return auto_delegation_code();
    }
    create.bytecode.clone()
}

/// Converts the consensus sequence update into the transaction carrier type.
pub fn sequence_update(account: Address, change: &ConfigChangeEntry) -> Eip8130SequenceUpdate {
    let sequence = config_change_sequence(account, change);
    Eip8130SequenceUpdate {
        slot: sequence.slot,
        is_multichain: sequence.is_multichain,
        new_value: sequence.new_value,
    }
}
