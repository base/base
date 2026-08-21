//! Protocol-injected `IAccountConfiguration` receipt logs for EIP-8130 account
//! changes.
//!
//! The enshrined apply path mutates `AccountConfiguration` storage outside an
//! EVM call frame, so it must inject the same events the Solidity contract
//! would emit (`ActorAuthorized`, `ActorRevoked`, `AccountCreated`) — plus
//! `DelegationApplied`, which the contract documents as protocol-injected only
//! (never emitted on the EVM path). Logs are written to the journal at
//! [`Eip8130Contracts::ACCOUNT_CONFIG`] and surface in the transaction receipt
//! ahead of any `calls` logs.

use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{SolEvent, sol};
use base_common_consensus::Eip8130Constants;
use base_precompile_storage::{ContractStorage, Result as StorageResult, StorageCtx};

use crate::{AccountConfigurationStorage, ActorConfig};

sol! {
    /// Events from `IAccountConfiguration` / EIP-8130 protocol-injected receipt logs.
    interface IAccountConfigurationEvents {
        /// Emitted when an actor is authorized (or upserted) on an account.
        event ActorAuthorized(address indexed account, bytes32 indexed actorId, bytes actorData);
        /// Emitted when an actor is revoked from an account.
        event ActorRevoked(address indexed account, bytes32 indexed actorId);
        /// Emitted when a counterfactual account is created.
        event AccountCreated(address indexed account, bytes32 userSalt, bytes32 codeHash);
        /// Protocol-injected receipt log for a successful delegation update
        /// (not emitted by the Solidity contract on the EVM path).
        event DelegationApplied(address indexed account, address target);
    }
}

pub use IAccountConfigurationEvents::{
    AccountCreated, ActorAuthorized, ActorRevoked, DelegationApplied,
};

/// Helpers for packing and emitting EIP-8130 protocol-injected account-change
/// logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountConfigurationEvents;

impl AccountConfigurationEvents {
    /// Packs `ActorAuthorized.actorData` as the finalized Keystore's
    /// `_emitActorAuthorized` does: `authenticator(20) ‖ expiry(6) ‖ scope(2) ‖
    /// reserved(4 zero bytes)` (32 bytes), plus `manager(20) ‖ commitment(32)`
    /// when `SCOPE_POLICY` is set (84 bytes total). This mirrors the right-aligned
    /// `ActorConfig` storage-slot field order (`authenticator ‖ expiry ‖ scope ‖
    /// reserved`) packed left-to-right via `abi.encodePacked`.
    #[must_use]
    pub fn pack_actor_data(config: &ActorConfig, manager: Address, commitment: B256) -> Bytes {
        let policy = config.scope & Eip8130Constants::SCOPE_POLICY != 0;
        let mut data = Vec::with_capacity(if policy { 84 } else { 32 });
        data.extend_from_slice(config.authenticator.as_slice());
        // uint48 expiry: low 6 bytes of the big-endian u64.
        data.extend_from_slice(&config.expiry.to_be_bytes()[2..]);
        // uint16 scope: 2 bytes big-endian.
        data.extend_from_slice(&config.scope.to_be_bytes());
        data.extend_from_slice(&[0u8; 4]);
        if policy {
            data.extend_from_slice(manager.as_slice());
            data.extend_from_slice(commitment.as_slice());
        }
        Bytes::from(data)
    }

    /// Emits [`ActorAuthorized`] from [`AccountConfigurationStorage::ADDRESS`].
    pub fn emit_actor_authorized(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
        config: &ActorConfig,
        manager: Address,
        commitment: B256,
    ) -> StorageResult<()> {
        storage.storage().emit_event(
            storage.address(),
            ActorAuthorized {
                account,
                actorId: actor_id,
                actorData: Self::pack_actor_data(config, manager, commitment),
            }
            .encode_log_data(),
        )
    }

    /// Emits [`ActorRevoked`] from [`AccountConfigurationStorage::ADDRESS`].
    pub fn emit_actor_revoked(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        actor_id: B256,
    ) -> StorageResult<()> {
        storage.storage().emit_event(
            storage.address(),
            ActorRevoked { account, actorId: actor_id }.encode_log_data(),
        )
    }

    /// Emits [`AccountCreated`] from [`AccountConfigurationStorage::ADDRESS`].
    ///
    /// `code_hash` is `keccak256` of the create entry's runtime bytecode (the
    /// same value Solidity's `createAccount` logs).
    pub fn emit_account_created(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        user_salt: B256,
        code_hash: B256,
    ) -> StorageResult<()> {
        storage.storage().emit_event(
            storage.address(),
            AccountCreated { account, userSalt: user_salt, codeHash: code_hash }.encode_log_data(),
        )
    }

    /// Emits [`DelegationApplied`] from the Account Configuration address.
    ///
    /// Takes a raw [`StorageCtx`] (unlike the other emit helpers) because the
    /// call sites — [`crate::DelegationEffect::install`] and auto-delegation —
    /// do not hold an [`AccountConfigurationStorage`] view. The log address is
    /// still [`AccountConfigurationStorage::ADDRESS`] so it cannot drift from
    /// the other emit helpers.
    ///
    /// Used for both explicit delegation entries and auto-delegation of a
    /// code-less sender to `DEFAULT_ACCOUNT`.
    pub fn emit_delegation_applied(
        sctx: StorageCtx<'_>,
        account: Address,
        target: Address,
    ) -> StorageResult<()> {
        sctx.emit_event(
            AccountConfigurationStorage::ADDRESS,
            DelegationApplied { account, target }.encode_log_data(),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn pack_actor_data_ungated_is_32_bytes() {
        let config = ActorConfig {
            authenticator: address!("0x00000000000000000000000000000000000000bb"),
            scope: Eip8130Constants::SCOPE_SENDER,
            expiry: 0x0102_0304_0506,
        };
        let data = AccountConfigurationEvents::pack_actor_data(&config, Address::ZERO, B256::ZERO);
        assert_eq!(data.len(), 32);
        assert_eq!(&data[..20], config.authenticator.as_slice());
        assert_eq!(&data[20..26], &config.expiry.to_be_bytes()[2..]);
        assert_eq!(&data[26..28], &Eip8130Constants::SCOPE_SENDER.to_be_bytes());
        assert_eq!(&data[28..], &[0u8; 4]);
    }

    #[test]
    fn pack_actor_data_policy_gated_is_84_bytes() {
        let config = ActorConfig {
            authenticator: address!("0x00000000000000000000000000000000000000bb"),
            scope: Eip8130Constants::SCOPE_POLICY,
            expiry: 0,
        };
        let manager = address!("0x00000000000000000000000000000000000000cc");
        let commitment = B256::repeat_byte(0x11);
        let data = AccountConfigurationEvents::pack_actor_data(&config, manager, commitment);
        assert_eq!(data.len(), 84);
        assert_eq!(&data[32..52], manager.as_slice());
        assert_eq!(&data[52..], commitment.as_slice());
    }
}
