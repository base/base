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
