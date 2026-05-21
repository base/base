use alloc::vec::Vec;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::guards::B20Guards;
use crate::{B20PausableFeature, B20TokenRole, IB20, Token, TokenAccounting};

/// Pause and unpause operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Pausable: Token {
    /// Returns whether the given pause `feature` is currently set.
    fn is_paused(&self, feature: IB20::PausableFeature) -> Result<bool> {
        Ok((self.accounting().paused()? & B20PausableFeature::mask(feature)) != U256::ZERO)
    }

    /// Returns all currently paused features.
    fn paused_features(&self) -> Result<Vec<IB20::PausableFeature>> {
        let paused = self.accounting().paused()?;
        let mut features = Vec::new();
        // REDEEM is reserved for a future redeem operation. It can be toggled and surfaced through
        // pausedFeatures, but no current B-20 operation checks it.
        for feature in [
            IB20::PausableFeature::TRANSFER,
            IB20::PausableFeature::MINT,
            IB20::PausableFeature::BURN,
            IB20::PausableFeature::REDEEM,
        ] {
            if (paused & B20PausableFeature::mask(feature)) != U256::ZERO {
                features.push(feature);
            }
        }
        Ok(features)
    }

    /// ORs `features` into the current paused bitmask.
    fn pause(
        &mut self,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        if features.is_empty() {
            return Err(BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
        }
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Pause)?;
        }
        let current = self.accounting().paused()?;
        let mut next = current;
        for feature in &features {
            next |= B20PausableFeature::mask(*feature);
        }
        self.accounting_mut().set_paused(next)?;
        self.accounting_mut()
            .emit_event(IB20::Paused { updater: caller, features }.encode_log_data())
    }

    /// Clears `features` from the current paused bitmask.
    fn unpause(
        &mut self,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        if features.is_empty() {
            return Err(BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
        }
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Unpause)?;
        }
        let mut next = self.accounting().paused()?;
        for feature in &features {
            next &= !B20PausableFeature::mask(*feature);
        }
        self.accounting_mut().set_paused(next)?;
        self.accounting_mut()
            .emit_event(IB20::Unpaused { updater: caller, features }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::Address;
    use base_precompile_storage::BasePrecompileError;

    use super::Pausable;
    use crate::{
        B20PausableFeature, B20TokenRole, IB20,
        common::{
            Token,
            test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
        },
    };

    const CALLER: Address = Address::repeat_byte(0xaa);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN_ADDR),
            InMemoryPolicy::new(),
        )
    }

    fn token_with_role(role: B20TokenRole, account: Address) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((role.id(), account), true);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn pause_sets_feature_and_emits_event() {
        let mut token = make_token();

        token.pause(CALLER, vec![IB20::PausableFeature::TRANSFER], true).unwrap();

        assert!(token.is_paused(IB20::PausableFeature::TRANSFER).unwrap());
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn pause_ors_multiple_features_into_existing_bitmask() {
        let mut token = make_token();

        token.pause(CALLER, vec![IB20::PausableFeature::TRANSFER], true).unwrap();
        token
            .pause(CALLER, vec![IB20::PausableFeature::MINT, IB20::PausableFeature::BURN], true)
            .unwrap();

        assert!(token.is_paused(IB20::PausableFeature::TRANSFER).unwrap());
        assert!(token.is_paused(IB20::PausableFeature::MINT).unwrap());
        assert!(token.is_paused(IB20::PausableFeature::BURN).unwrap());
    }

    #[test]
    fn unpause_clears_selected_feature_and_leaves_others_paused() {
        let mut token = make_token();

        token
            .pause(CALLER, vec![IB20::PausableFeature::TRANSFER, IB20::PausableFeature::MINT], true)
            .unwrap();
        token.unpause(CALLER, vec![IB20::PausableFeature::MINT], true).unwrap();

        assert!(token.is_paused(IB20::PausableFeature::TRANSFER).unwrap());
        assert!(!token.is_paused(IB20::PausableFeature::MINT).unwrap());
    }

    #[test]
    fn paused_features_returns_active_features_in_abi_order() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::TRANSFER)
            | B20PausableFeature::mask(IB20::PausableFeature::BURN);
        let token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.paused_features().unwrap(),
            vec![IB20::PausableFeature::TRANSFER, IB20::PausableFeature::BURN]
        );
    }

    #[test]
    fn pause_empty_feature_set_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.pause(CALLER, vec![], true).unwrap_err(),
            BasePrecompileError::revert(IB20::EmptyFeatureSet {})
        );
    }

    #[test]
    fn unpause_empty_feature_set_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.unpause(CALLER, vec![], true).unwrap_err(),
            BasePrecompileError::revert(IB20::EmptyFeatureSet {})
        );
    }

    #[test]
    fn non_privileged_pause_without_role_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.pause(CALLER, vec![IB20::PausableFeature::TRANSFER], false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Pause.id(),
            })
        );
    }

    #[test]
    fn non_privileged_pause_with_role_succeeds() {
        let mut token = token_with_role(B20TokenRole::Pause, CALLER);

        token.pause(CALLER, vec![IB20::PausableFeature::TRANSFER], false).unwrap();

        assert!(token.is_paused(IB20::PausableFeature::TRANSFER).unwrap());
    }

    #[test]
    fn non_privileged_unpause_without_role_reverts() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::TRANSFER);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.unpause(CALLER, vec![IB20::PausableFeature::TRANSFER], false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Unpause.id(),
            })
        );
    }

    #[test]
    fn non_privileged_unpause_with_role_succeeds() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::TRANSFER);
        accounting.roles.insert((B20TokenRole::Unpause.id(), CALLER), true);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        token.unpause(CALLER, vec![IB20::PausableFeature::TRANSFER], false).unwrap();

        assert!(!token.is_paused(IB20::PausableFeature::TRANSFER).unwrap());
    }
}
