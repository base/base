use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{CAPABILITY_PAUSABLE, IB20, Token, TokenAccounting};

/// Pause and unpause operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Pausable: Token {
    /// Returns whether the given pause `vector` bit is currently set.
    fn is_paused(&self, vector: U256) -> Result<bool> {
        Ok((self.accounting().paused()? & vector) != U256::ZERO)
    }

    /// Returns whether the `PAUSABLE` capability bit is set on this token.
    fn is_pausable(&self) -> Result<bool> {
        Ok((self.accounting().capabilities()? & CAPABILITY_PAUSABLE) != U256::ZERO)
    }

    /// ORs `vectors` into the current paused bitmask. Requires `PAUSABLE` capability.
    /// Emits `Paused(caller, vectors)`.
    fn pause(&mut self, caller: Address, vectors: U256) -> Result<()> {
        if vectors == U256::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidAmount {}));
        }
        if !self.is_pausable()? {
            return Err(BasePrecompileError::revert(IB20::FeatureDisabled {
                capability: CAPABILITY_PAUSABLE,
            }));
        }
        let current = self.accounting().paused()?;
        self.accounting_mut().set_paused(current | vectors)?;
        self.accounting_mut()
            .emit_event(IB20::Paused { updater: caller, vectors }.encode_log_data())
    }

    /// Clears all paused vectors. Requires `PAUSABLE` capability.
    /// Emits `Unpaused(caller)`.
    fn unpause(&mut self, caller: Address) -> Result<()> {
        if !self.is_pausable()? {
            return Err(BasePrecompileError::revert(IB20::FeatureDisabled {
                capability: CAPABILITY_PAUSABLE,
            }));
        }
        self.accounting_mut().set_paused(U256::ZERO)?;
        self.accounting_mut().emit_event(IB20::Unpaused { updater: caller }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::Pausable;
    use crate::common::{
        CAPABILITY_PAUSABLE, Token,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const CALLER: Address = Address::repeat_byte(0xaa);
    const VECTOR_1: U256 = U256::from_limbs([1, 0, 0, 0]);
    const VECTOR_2: U256 = U256::from_limbs([2, 0, 0, 0]);

    fn make_token(caps: U256) -> TestToken {
        let mut acc = InMemoryTokenAccounting::new(Address::repeat_byte(1));
        acc.capabilities = caps;
        TestToken::with_storage_and_policy(acc, InMemoryPolicy::new())
    }

    #[test]
    fn is_pausable_reflects_capability_bit() {
        assert!(make_token(CAPABILITY_PAUSABLE).is_pausable().unwrap());
        assert!(!make_token(U256::ZERO).is_pausable().unwrap());
    }

    #[test]
    fn pause_sets_bitmask_and_emits_event() {
        let mut token = make_token(CAPABILITY_PAUSABLE);
        token.pause(CALLER, VECTOR_1).unwrap();

        assert!(token.is_paused(VECTOR_1).unwrap());
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn pause_ors_into_existing_bitmask() {
        let mut token = make_token(CAPABILITY_PAUSABLE);
        token.pause(CALLER, VECTOR_1).unwrap();
        token.pause(CALLER, VECTOR_2).unwrap();

        assert!(token.is_paused(VECTOR_1).unwrap());
        assert!(token.is_paused(VECTOR_2).unwrap());
    }

    #[test]
    fn unpause_clears_all_vectors() {
        let mut token = make_token(CAPABILITY_PAUSABLE);
        token.pause(CALLER, VECTOR_1 | VECTOR_2).unwrap();
        token.unpause(CALLER).unwrap();

        assert!(!token.is_paused(VECTOR_1).unwrap());
        assert!(!token.is_paused(VECTOR_2).unwrap());
    }

    #[test]
    fn pause_without_capability_reverts() {
        let mut token = make_token(U256::ZERO);
        assert!(token.pause(CALLER, VECTOR_1).is_err());
    }

    #[test]
    fn unpause_without_capability_reverts() {
        let mut token = make_token(U256::ZERO);
        assert!(token.unpause(CALLER).is_err());
    }

    #[test]
    fn pause_zero_vector_reverts() {
        let mut token = make_token(CAPABILITY_PAUSABLE);
        assert!(token.pause(CALLER, U256::ZERO).is_err());
    }
}
