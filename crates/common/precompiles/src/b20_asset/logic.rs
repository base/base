//! Fork-versioned `transfer` behavior for the B-20 asset token (execution-consensus Case 1).
//!
//! Design C (pure free-function version modules, selected by a fork VALUE):
//! - The active fork is resolved once into a [`Version`] value (captured at precompile install) and
//!   threaded into dispatch. There is NO version type parameter on the token.
//! - Each version is a FROZEN module of pure free functions ([`v1`], [`v2`]). Within a version,
//!   functions call their siblings by module-relative name, so a version is a self-contained, whole
//!   unit — a v2 shell can never wrap a v1 core (the cluster split-brain is unrepresentable).
//! - Dispatch selects ONE version per call (whole-version selection) via the `versioned!` match in
//!   `dispatch.rs`; per-method/per-selector routing is intentionally not offered.
//!
//! This is a proof of concept scoped to the `transfer` cluster only. `v1` == today's behavior
//! (a verbatim port of `common/ops/transferable.rs`); `v2` demonstrates a future hard-fork change
//! (reject zero-amount transfers). Every other method stays unversioned in this PoC.

use alloy_primitives::{Address, B256, U256};
use base_common_chains::BaseUpgrade;
use base_precompile_storage::Result;

use crate::Token;

/// Identifies which frozen behavior generation a fork runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// Genesis behavior (Beryl and earlier) — [`v1`].
    V1,
    /// Next generation (Cobalt, in this PoC) — [`v2`].
    V2,
}

/// Maps a resolved [`BaseUpgrade`] to the behavior generation active at that fork.
///
/// The single source of truth for the fork -> version binding. For this PoC, Cobalt (the latest
/// fork) runs [`Version::V2`] and every earlier fork runs [`Version::V1`]. Adding a fork's change is
/// a one-line edit here (plus a new `vN` module).
///
/// NOTE: remapping a *shipped* fork is itself a consensus change; the Cobalt mapping is a
/// draft/demonstration only.
pub fn for_upgrade(upgrade: BaseUpgrade) -> Version {
    if upgrade >= BaseUpgrade::Cobalt { Version::V2 } else { Version::V1 }
}

// --- Version dispatch: plain functions that route a resolved `Version` to the frozen module.
// Whole-version selection — one `Version` per call, every cluster method routed the same way.

/// Runs `transfer` under behavior generation `version`.
pub(crate) fn transfer<T: Token>(
    version: Version,
    t: &mut T,
    from: Address,
    to: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    match version {
        Version::V1 => v1::transfer(t, from, to, amount, privileged),
        Version::V2 => v2::transfer(t, from, to, amount, privileged),
    }
}

/// Runs `transfer_from` under behavior generation `version`.
pub(crate) fn transfer_from<T: Token>(
    version: Version,
    t: &mut T,
    spender: Address,
    from: Address,
    to: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    match version {
        Version::V1 => v1::transfer_from(t, spender, from, to, amount, privileged),
        Version::V2 => v2::transfer_from(t, spender, from, to, amount, privileged),
    }
}

/// Runs `transfer_with_memo` under behavior generation `version`.
pub(crate) fn transfer_with_memo<T: Token>(
    version: Version,
    t: &mut T,
    from: Address,
    to: Address,
    amount: U256,
    memo: B256,
    privileged: bool,
) -> Result<()> {
    match version {
        Version::V1 => v1::transfer_with_memo(t, from, to, amount, memo, privileged),
        Version::V2 => v2::transfer_with_memo(t, from, to, amount, memo, privileged),
    }
}

/// Runs `transfer_from_with_memo` under behavior generation `version`.
pub(crate) fn transfer_from_with_memo<T: Token>(
    version: Version,
    t: &mut T,
    spender: Address,
    from: Address,
    to: Address,
    amount: U256,
    memo: B256,
    privileged: bool,
) -> Result<()> {
    match version {
        Version::V1 => v1::transfer_from_with_memo(t, spender, from, to, amount, memo, privileged),
        Version::V2 => v2::transfer_from_with_memo(t, spender, from, to, amount, memo, privileged),
    }
}

/// Genesis behavior — a verbatim port of `common/ops/transferable.rs`. FROZEN once shipped.
pub(crate) mod v1 {
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{BasePrecompileError, Result};

    use crate::{B20Guards, B20PolicyType, IB20, Token, TokenAccounting};

    /// ERC-20 transfer: pause check, then [`transfer_inner`].
    pub(crate) fn transfer<T: Token>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused::<T>(t, IB20::PausableFeature::TRANSFER)?;
        transfer_inner(t, from, to, amount, privileged)
    }

    /// Shared transfer sink: address guards, policy checks, balance math, `Transfer` event.
    pub(crate) fn transfer_inner<T: Token>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        if !privileged {
            B20Guards::ensure_policy_type::<T>(t, B20PolicyType::TransferSender, from)?;
            B20Guards::ensure_policy_type::<T>(t, B20PolicyType::TransferReceiver, to)?;
        }
        let from_balance = t.accounting().balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance: from_balance,
                needed: amount,
            }));
        }
        let new_from_balance =
            from_balance.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        t.accounting_mut().set_balance(from, new_from_balance)?;
        let to_balance = t.accounting().balance_of(to)?;
        let new_to_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        t.accounting_mut().set_balance(to, new_to_balance)?;
        t.accounting_mut().emit_event(IB20::Transfer { from, to, amount }.encode_log_data())
    }

    /// ERC-20 transferFrom: allowance + executor policy, then [`transfer_inner`].
    pub(crate) fn transfer_from<T: Token>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused::<T>(t, IB20::PausableFeature::TRANSFER)?;
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        let allowance = t.accounting().allowance(from, spender)?;
        let is_infinite = allowance == U256::MAX;
        if !is_infinite && allowance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender,
                allowance,
                needed: amount,
            }));
        }
        if !privileged && spender != from {
            B20Guards::ensure_policy_type::<T>(t, B20PolicyType::TransferExecutor, spender)?;
        }
        transfer_inner(t, from, to, amount, privileged)?;
        if is_infinite {
            return Ok(());
        }
        t.accounting_mut().set_allowance(from, spender, allowance - amount)
    }

    /// [`transfer`] followed by a `Memo` event.
    pub(crate) fn transfer_with_memo<T: Token>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        transfer(t, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: from, memo }.encode_log_data())
    }

    /// [`transfer_from`] followed by a `Memo` event.
    pub(crate) fn transfer_from_with_memo<T: Token>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        transfer_from(t, spender, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: spender, memo }.encode_log_data())
    }
}

/// Next-generation behavior (Cobalt in this PoC): identical to [`v1`] except `transfer_inner`
/// rejects zero-amount transfers. FROZEN once shipped.
///
/// The whole cluster is restated so every function's `transfer_inner` call binds to *this* module's
/// core (module-relative). A v2 wrapper can therefore never run v1's core.
pub(crate) mod v2 {
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{BasePrecompileError, Result};

    use crate::{B20Guards, B20PolicyType, IB20, Token, TokenAccounting};

    /// ERC-20 transfer: pause check, then [`transfer_inner`].
    pub(crate) fn transfer<T: Token>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused::<T>(t, IB20::PausableFeature::TRANSFER)?;
        transfer_inner(t, from, to, amount, privileged)
    }

    /// Shared transfer sink. ★ V2 change: rejects zero-amount transfers with `InvalidAmount`.
    pub(crate) fn transfer_inner<T: Token>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        // ★ V2 behavior change vs v1.
        if amount.is_zero() {
            return Err(BasePrecompileError::revert(IB20::InvalidAmount {}));
        }
        if !privileged {
            B20Guards::ensure_policy_type::<T>(t, B20PolicyType::TransferSender, from)?;
            B20Guards::ensure_policy_type::<T>(t, B20PolicyType::TransferReceiver, to)?;
        }
        let from_balance = t.accounting().balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance: from_balance,
                needed: amount,
            }));
        }
        let new_from_balance =
            from_balance.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        t.accounting_mut().set_balance(from, new_from_balance)?;
        let to_balance = t.accounting().balance_of(to)?;
        let new_to_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        t.accounting_mut().set_balance(to, new_to_balance)?;
        t.accounting_mut().emit_event(IB20::Transfer { from, to, amount }.encode_log_data())
    }

    /// ERC-20 transferFrom: allowance + executor policy, then [`transfer_inner`].
    pub(crate) fn transfer_from<T: Token>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused::<T>(t, IB20::PausableFeature::TRANSFER)?;
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        let allowance = t.accounting().allowance(from, spender)?;
        let is_infinite = allowance == U256::MAX;
        if !is_infinite && allowance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender,
                allowance,
                needed: amount,
            }));
        }
        if !privileged && spender != from {
            B20Guards::ensure_policy_type::<T>(t, B20PolicyType::TransferExecutor, spender)?;
        }
        transfer_inner(t, from, to, amount, privileged)?;
        if is_infinite {
            return Ok(());
        }
        t.accounting_mut().set_allowance(from, spender, allowance - amount)
    }

    /// [`transfer`] followed by a `Memo` event.
    pub(crate) fn transfer_with_memo<T: Token>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        transfer(t, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: from, memo }.encode_log_data())
    }

    /// [`transfer_from`] followed by a `Memo` event.
    pub(crate) fn transfer_from_with_memo<T: Token>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        transfer_from(t, spender, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: spender, memo }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_chains::BaseUpgrade;
    use base_precompile_storage::BasePrecompileError;

    use super::{Version, for_upgrade, v1, v2};
    use crate::{
        B20AssetToken, IB20, InMemoryPolicy, InMemoryTokenAccounting, Token, TokenAccounting,
    };

    type TestAssetToken = B20AssetToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);

    fn token_with_balance(balance: U256) -> TestAssetToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.balances.insert(ALICE, balance);
        TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn v1_allows_zero_amount_transfer() {
        let mut token = token_with_balance(U256::from(100u64));

        v1::transfer(&mut token, ALICE, BOB, U256::ZERO, false).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn v2_rejects_zero_amount_transfer() {
        let mut token = token_with_balance(U256::from(100u64));

        let err = v2::transfer(&mut token, ALICE, BOB, U256::ZERO, false).unwrap_err();

        assert_eq!(err, BasePrecompileError::revert(IB20::InvalidAmount {}));
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 0);
    }

    #[test]
    fn v1_and_v2_agree_on_nonzero_transfer() {
        let mut a = token_with_balance(U256::from(100u64));
        let mut b = token_with_balance(U256::from(100u64));

        v1::transfer(&mut a, ALICE, BOB, U256::from(40u64), false).unwrap();
        v2::transfer(&mut b, ALICE, BOB, U256::from(40u64), false).unwrap();

        assert_eq!(
            a.accounting().balance_of(ALICE).unwrap(),
            b.accounting().balance_of(ALICE).unwrap()
        );
        assert_eq!(a.accounting().balance_of(BOB).unwrap(), b.accounting().balance_of(BOB).unwrap());
        assert_eq!(a.accounting().events.len(), b.accounting().events.len());
    }

    #[test]
    fn for_upgrade_selects_v2_at_cobalt_and_v1_before() {
        for upgrade in BaseUpgrade::VARIANTS {
            let expected =
                if *upgrade >= BaseUpgrade::Cobalt { Version::V2 } else { Version::V1 };
            assert_eq!(for_upgrade(*upgrade), expected);
        }
        assert_eq!(for_upgrade(BaseUpgrade::Beryl), Version::V1);
        assert_eq!(for_upgrade(BaseUpgrade::Cobalt), Version::V2);
    }
}
