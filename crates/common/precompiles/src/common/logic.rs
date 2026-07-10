//! Fork-versioned behavior for the B-20 asset token (execution-consensus Case 1).
//!
//! Precompile logic is native Rust in the node binary, so changing a method's behavior across a
//! hard fork would make an old transaction replayed at its historical block produce different
//! state — a consensus fork. This module makes the `transfer` cluster's behavior a pure function of
//! the active [`BaseUpgrade`]: each generation of behavior is a zero-sized type implementing
//! [`AssetLogic`], and [`asset_logic_for`] maps a fork to its generation. The version is selected
//! once at precompile construction and carried as the `L` type parameter on
//! [`B20AssetToken`](crate::B20AssetToken), so the hot path stays fully monomorphized — no vtable,
//! no per-call fork branch.
//!
//! This is a walking-skeleton sample: only the `transfer` cluster is versioned. `AssetLogicV1` is
//! today's behavior (it forwards to the existing [`Transferable`] methods, so it is byte-identical
//! by construction), and `AssetLogicV2` demonstrates a future change.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_common_chains::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{B20Guards, B20PolicyType, IB20, TokenAccounting, Transferable};

/// A frozen generation of B-20 asset behavior.
///
/// Every method is version-explicit: implementers compose intra-cluster helpers through `Self::`
/// (never through the token's [`Transferable`] methods), so a versioned method never accidentally
/// calls a different generation's helper. Implementers are zero-sized and `'static`, so selecting a
/// version costs nothing at runtime and the token's calls monomorphize.
pub trait AssetLogic: Copy + 'static {
    /// ERC-20 `transfer`: pause check, then [`Self::transfer_inner`].
    fn transfer<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Shared transfer sink: address/amount guards, policy checks, balance math, `Transfer` event.
    fn transfer_inner<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `transferFrom`: allowance + executor policy, then [`Self::transfer_inner`].
    fn transfer_from<T: Transferable>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// [`Self::transfer`] followed by a `Memo` event.
    fn transfer_with_memo<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()>;

    /// [`Self::transfer_from`] followed by a `Memo` event.
    fn transfer_from_with_memo<T: Transferable>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()>;
}

/// Genesis behavior (Beryl onward) — identical to today's [`Transferable`] implementation.
///
/// Each method forwards to the existing trait method, so V1 is today's behavior *by reference*:
/// there is no transcription risk and `common/ops/transferable.rs` stays the source of truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetLogicV1;

impl AssetLogic for AssetLogicV1 {
    fn transfer<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        Transferable::transfer(t, from, to, amount, privileged)
    }

    fn transfer_inner<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        Transferable::transfer_inner(t, from, to, amount, privileged)
    }

    fn transfer_from<T: Transferable>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        Transferable::transfer_from(t, spender, from, to, amount, privileged)
    }

    fn transfer_with_memo<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        Transferable::transfer_with_memo(t, from, to, amount, memo, privileged)
    }

    fn transfer_from_with_memo<T: Transferable>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        Transferable::transfer_from_with_memo(t, spender, from, to, amount, memo, privileged)
    }
}

/// Illustrative next-generation behavior: `transfer_inner` additionally rejects zero-amount
/// transfers with `InvalidAmount`. Everything else matches V1.
///
/// Note the whole cluster is restated so `Self::transfer_inner` binds to *this* generation:
/// `transfer` / `transfer_from` / the memo wrappers cannot delegate to `AssetLogicV1` or their
/// internal calls would rebind to V1's inner — the "cluster rule". Only `transfer_inner` carries
/// new logic; the wrappers are mechanical copies of V1's shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetLogicV2;

impl AssetLogic for AssetLogicV2 {
    fn transfer<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused::<T>(t, IB20::PausableFeature::TRANSFER)?;
        Self::transfer_inner(t, from, to, amount, privileged)
    }

    fn transfer_inner<T: Transferable>(
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
        // ★ V2 behavior change: reject zero-amount transfers.
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

    fn transfer_from<T: Transferable>(
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
        Self::transfer_inner(t, from, to, amount, privileged)?;
        if is_infinite {
            return Ok(());
        }
        t.accounting_mut().set_allowance(from, spender, allowance - amount)
    }

    fn transfer_with_memo<T: Transferable>(
        t: &mut T,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        Self::transfer(t, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: from, memo }.encode_log_data())
    }

    fn transfer_from_with_memo<T: Transferable>(
        t: &mut T,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        Self::transfer_from(t, spender, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: spender, memo }.encode_log_data())
    }
}

/// Identifies which [`AssetLogic`] generation a fork resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetLogicId {
    /// Genesis behavior — [`AssetLogicV1`].
    V1,
    /// Next generation — [`AssetLogicV2`].
    V2,
}

/// Maps a [`BaseUpgrade`] to the asset-logic generation active at that fork.
///
/// This is the single place a fork's transfer behavior is bound — adding a fork's new behavior is a
/// one-line change here. For this sample, Cobalt (the latest fork) runs [`AssetLogicV2`] and every
/// earlier fork runs [`AssetLogicV1`], so the fork boundary is exercised through the real
/// lookup/dispatch path and not only by the unit tests below.
///
/// NOTE: remapping a *shipped* fork to new behavior is itself a real consensus change; this Cobalt
/// mapping is a draft/demonstration only and is not intended to ship as-is.
pub fn asset_logic_for(upgrade: BaseUpgrade) -> AssetLogicId {
    if upgrade >= BaseUpgrade::Cobalt {
        AssetLogicId::V2
    } else {
        AssetLogicId::V1
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_chains::BaseUpgrade;
    use base_precompile_storage::BasePrecompileError;

    use super::{AssetLogic, AssetLogicId, AssetLogicV1, AssetLogicV2, asset_logic_for};
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

        AssetLogicV1::transfer(&mut token, ALICE, BOB, U256::ZERO, false).unwrap();

        // V1 == today: a zero-amount transfer succeeds and still emits `Transfer`.
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn v2_rejects_zero_amount_transfer() {
        let mut token = token_with_balance(U256::from(100u64));

        let err = AssetLogicV2::transfer(&mut token, ALICE, BOB, U256::ZERO, false).unwrap_err();

        // V2's behavior change: zero-amount transfers revert with `InvalidAmount`, no state/event.
        assert_eq!(err, BasePrecompileError::revert(IB20::InvalidAmount {}));
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 0);
    }

    #[test]
    fn v1_and_v2_agree_on_nonzero_transfer() {
        let mut v1_token = token_with_balance(U256::from(100u64));
        let mut v2_token = token_with_balance(U256::from(100u64));

        AssetLogicV1::transfer(&mut v1_token, ALICE, BOB, U256::from(40u64), false).unwrap();
        AssetLogicV2::transfer(&mut v2_token, ALICE, BOB, U256::from(40u64), false).unwrap();

        // Only the zero-amount case diverges; a normal transfer is identical under both versions.
        assert_eq!(
            v1_token.accounting().balance_of(ALICE).unwrap(),
            v2_token.accounting().balance_of(ALICE).unwrap(),
        );
        assert_eq!(
            v1_token.accounting().balance_of(BOB).unwrap(),
            v2_token.accounting().balance_of(BOB).unwrap(),
        );
        assert_eq!(v1_token.accounting().events.len(), v2_token.accounting().events.len());
    }

    #[test]
    fn asset_logic_for_selects_v2_at_cobalt_and_v1_before() {
        // Exhaustiveness guard (mirrors `test_all_base_upgrades_have_precompile_sets`): every fork
        // resolves to a defined generation, and the Cobalt boundary is pinned.
        for upgrade in BaseUpgrade::VARIANTS {
            let expected = if *upgrade >= BaseUpgrade::Cobalt {
                AssetLogicId::V2
            } else {
                AssetLogicId::V1
            };
            assert_eq!(asset_logic_for(*upgrade), expected);
        }
        assert_eq!(asset_logic_for(BaseUpgrade::Beryl), AssetLogicId::V1);
        assert_eq!(asset_logic_for(BaseUpgrade::Cobalt), AssetLogicId::V2);
    }
}
