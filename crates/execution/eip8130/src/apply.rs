//! The EIP-8130 account-changes apply step for the pure-secp256k1 transaction
//! type: the only account change is an [EIP-7702]-style [`DelegationEffect`].
//!
//! A delegation touches the *account's code* rather than any protocol storage,
//! so applying it is surfaced as an [`AppliedAccountChanges`] the execution layer
//! (which holds the account/state-trie handle) installs via
//! [`DelegationEffect::install`].
//!
//! [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702

use alloy_primitives::{Address, Bytes};
use base_common_consensus::Eip8130Constants;
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::state::Bytecode;

/// Reason a delegation account change could not be applied.
///
/// Every variant is a hard rejection while applying EIP-8130 state changes: a
/// transaction MUST NOT be included if applying its account changes fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    /// An EIP-8130 state read or write failed.
    #[error("EIP-8130 state access failed: {0}")]
    Storage(#[from] BasePrecompileError),

    /// More than one delegation entry in a single transaction.
    #[error("at most one delegation entry is allowed")]
    MultipleDelegations,

    /// A delegation attempted to replace ordinary contract bytecode. Delegation
    /// may replace only empty code or code beginning with the delegation
    /// indicator prefix.
    #[error("delegation cannot replace non-delegation code at account {account}")]
    NonDelegatableCode {
        /// The account whose existing code cannot be replaced by a delegation.
        account: Address,
    },
}

/// A delegation's deferred code write against an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DelegationEffect {
    /// The account whose code the delegation indicator is written to (cleared).
    pub account: Address,
    /// The delegation target; `address(0)` clears the existing delegation.
    pub target: Address,
}

impl DelegationEffect {
    /// Creates a deferred delegation code effect.
    #[must_use]
    pub const fn new(account: Address, target: Address) -> Self {
        Self { account, target }
    }

    /// Returns whether `code` may be replaced by a delegation entry.
    ///
    /// Empty code and any code beginning with the delegation indicator prefix
    /// are replaceable. The prefix match is intentional: this does not require
    /// the code to have the canonical 23-byte indicator length.
    #[must_use]
    pub fn can_replace_code(code: &[u8]) -> bool {
        code.is_empty() || code.starts_with(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX)
    }

    /// Installs or clears this delegation after verifying the account's current
    /// code is delegatable.
    ///
    /// The current full bytecode is read before any code write. Ordinary
    /// contract bytecode is left unchanged and rejected with
    /// [`ApplyError::NonDelegatableCode`].
    pub fn install(&self, sctx: StorageCtx<'_>) -> Result<(), ApplyError> {
        let can_replace = sctx.with_account_code(self.account, |code| {
            Ok(Self::can_replace_code(code.original_bytes().as_ref()))
        })?;
        if !can_replace {
            return Err(ApplyError::NonDelegatableCode { account: self.account });
        }

        let code = if self.target.is_zero() {
            Bytecode::default()
        } else {
            Bytecode::new_eip7702(self.target)
        };
        sctx.set_code(self.account, code)?;
        Ok(())
    }

    /// The delegation-indicator code to install
    /// (`DELEGATION_INDICATOR_PREFIX || target`), or `None` to clear the
    /// account's delegation (a zero target).
    #[must_use]
    pub fn indicator(&self) -> Option<Bytes> {
        if self.target.is_zero() {
            return None;
        }
        let mut code = Vec::with_capacity(Eip8130Constants::DELEGATION_INDICATOR_SIZE);
        code.extend_from_slice(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX);
        code.extend_from_slice(self.target.as_slice());
        Some(Bytes::from(code))
    }
}

/// The deferred account-*code* effects produced by applying a transaction's
/// account changes. These are the writes the execution layer must perform
/// against the account/state trie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AppliedAccountChanges {
    /// The delegation set or cleared by a delegation entry, if any.
    pub delegation: Option<DelegationEffect>,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use base_precompile_storage::{HashMapStorageProvider, PrecompileStorageProvider};

    use super::*;

    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000a1");

    #[test]
    fn delegation_effect_indicator_set_and_clear() {
        let target = address!("0x00000000000000000000000000000000000000ee");
        let set = DelegationEffect::new(ACCOUNT, target);
        let code = set.indicator().unwrap();
        assert_eq!(code.len(), Eip8130Constants::DELEGATION_INDICATOR_SIZE);
        assert_eq!(&code[..3], &Eip8130Constants::DELEGATION_INDICATOR_PREFIX);
        assert_eq!(&code[3..], target.as_slice());

        let clear = DelegationEffect::new(ACCOUNT, Address::ZERO);
        assert!(clear.indicator().is_none());
    }

    #[test]
    fn delegation_effect_replaceable_code_predicate() {
        assert!(DelegationEffect::can_replace_code(&[]));
        assert!(DelegationEffect::can_replace_code(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX));

        let mut full_indicator = Eip8130Constants::DELEGATION_INDICATOR_PREFIX.to_vec();
        full_indicator.extend_from_slice(Address::repeat_byte(0x11).as_slice());
        assert!(DelegationEffect::can_replace_code(&full_indicator));

        assert!(!DelegationEffect::can_replace_code(&[0x60, 0x00]));
        assert!(!DelegationEffect::can_replace_code(&[0xef, 0x01, 0x01]));
    }

    #[test]
    fn delegation_effect_install_rejects_ordinary_code_without_mutating_it() {
        let ordinary = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, ordinary.clone()).unwrap();

        let effect = DelegationEffect::new(ACCOUNT, Address::repeat_byte(0x22));
        let error = StorageCtx::enter(&mut storage, |sctx| effect.install(sctx)).unwrap_err();

        assert_eq!(error, ApplyError::NonDelegatableCode { account: ACCOUNT });
        assert_eq!(
            storage.get_account_info(ACCOUNT).and_then(|info| info.code.as_ref()),
            Some(&ordinary)
        );
    }

    #[test]
    fn delegation_effect_install_accepts_empty_code() {
        let target = Address::repeat_byte(0x33);
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap();

        assert_eq!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address),
            Some(target)
        );
    }

    #[test]
    fn delegation_effect_install_updates_existing_delegation() {
        let target = Address::repeat_byte(0x44);
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, Bytecode::new_eip7702(Address::repeat_byte(0x11))).unwrap();

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap();

        assert_eq!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address),
            Some(target)
        );
    }

    #[test]
    fn delegation_effect_install_clears_existing_delegation() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, Bytecode::new_eip7702(Address::repeat_byte(0x11))).unwrap();

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, Address::ZERO).install(sctx)
        })
        .unwrap();

        assert!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .is_some_and(Bytecode::is_empty)
        );
    }

    #[test]
    fn delegation_effect_install_allows_redelegation_of_existing_delegate() {
        let target = Address::repeat_byte(0x57);
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, Bytecode::new_eip7702(Address::repeat_byte(0x11))).unwrap();

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, target).install(sctx)
        })
        .unwrap();

        assert_eq!(
            storage
                .get_account_info(ACCOUNT)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address),
            Some(target)
        );
    }
}
