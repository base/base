//! The EIP-8130 delegation entry's code effect: setting or clearing an
//! [EIP-7702]-style delegation indicator on the sender.
//!
//! [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702

use alloy_primitives::Address;
use base_common_consensus::Eip8130Constants;
use base_precompile_storage::{BasePrecompileError, Bytecode, StorageCtx};

/// Reason a delegation entry is rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegationError {
    /// Reading the account's code or writing the indicator failed.
    #[error("EIP-8130 state access failed: {0}")]
    Storage(#[from] BasePrecompileError),

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

    /// Checks that the account's current code may be replaced by this
    /// delegation, as [`Self::can_replace_code`] defines.
    ///
    /// Mempool admission and execution both apply this check.
    ///
    /// # Errors
    /// Returns [`DelegationError::NonDelegatableCode`] when the account holds
    /// ordinary contract bytecode, or [`DelegationError::Storage`] when its
    /// code cannot be read.
    pub fn check(&self, sctx: StorageCtx<'_>) -> Result<(), DelegationError> {
        let can_replace = sctx.with_account_code(self.account, |code| {
            Ok(Self::can_replace_code(code.original_bytes().as_ref()))
        })?;
        if !can_replace {
            return Err(DelegationError::NonDelegatableCode { account: self.account });
        }
        Ok(())
    }

    /// Installs or clears this delegation after [`Self::check`] accepts the
    /// account's current code.
    ///
    /// Ordinary contract bytecode is left unchanged.
    pub fn install(&self, sctx: StorageCtx<'_>) -> Result<(), DelegationError> {
        self.check(sctx)?;

        let code = if self.target.is_zero() {
            Bytecode::default()
        } else {
            Bytecode::new_eip7702(self.target)
        };
        sctx.set_code(self.account, code)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};
    use base_precompile_storage::{HashMapStorageProvider, PrecompileStorageProvider};

    use super::*;

    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000a1");

    #[test]
    fn replaceable_code_predicate() {
        assert!(DelegationEffect::can_replace_code(&[]));
        assert!(DelegationEffect::can_replace_code(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX));

        let mut full_indicator = Eip8130Constants::DELEGATION_INDICATOR_PREFIX.to_vec();
        full_indicator.extend_from_slice(Address::repeat_byte(0x11).as_slice());
        assert!(DelegationEffect::can_replace_code(&full_indicator));

        assert!(!DelegationEffect::can_replace_code(&[0x60, 0x00]));
        assert!(!DelegationEffect::can_replace_code(&[0xef, 0x01, 0x01]));
    }

    #[test]
    fn install_rejects_ordinary_code_without_mutating_it() {
        let ordinary = Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00]));
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, ordinary.clone()).unwrap();

        let effect = DelegationEffect::new(ACCOUNT, Address::repeat_byte(0x22));
        let error = StorageCtx::enter(&mut storage, |sctx| effect.install(sctx)).unwrap_err();

        assert_eq!(error, DelegationError::NonDelegatableCode { account: ACCOUNT });
        assert_eq!(storage.get_code(ACCOUNT), Some(&ordinary));
    }

    #[test]
    fn install_sets_indicator_on_replaceable_code() {
        // Empty-code accounts and accounts already carrying a delegation may be
        // (re-)delegated: the only code-shape guard rejects ordinary bytecode.
        let target = Address::repeat_byte(0x44);
        for existing in [None, Some(Bytecode::new_eip7702(Address::repeat_byte(0x11)))] {
            let mut storage = HashMapStorageProvider::new(1);
            if let Some(code) = existing {
                storage.set_code(ACCOUNT, code).unwrap();
            }

            StorageCtx::enter(&mut storage, |sctx| {
                DelegationEffect::new(ACCOUNT, target).install(sctx)
            })
            .unwrap();

            assert_eq!(storage.get_code(ACCOUNT).and_then(Bytecode::eip7702_address), Some(target));
        }
    }

    #[test]
    fn install_clears_existing_delegation() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_code(ACCOUNT, Bytecode::new_eip7702(Address::repeat_byte(0x11))).unwrap();

        StorageCtx::enter(&mut storage, |sctx| {
            DelegationEffect::new(ACCOUNT, Address::ZERO).install(sctx)
        })
        .unwrap();

        assert!(storage.get_code(ACCOUNT).is_some_and(Bytecode::is_empty));
    }
}
