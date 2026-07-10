//! Fork-versioned behavior for the B-20 asset token (execution-consensus Case 1).
//!
//! Precompile logic is native Rust in the node binary, so changing a method's behavior across a
//! hard fork would make an old transaction replayed at its historical block produce different
//! state — a consensus fork. This module makes B-20 asset behavior a pure function of the active
//! [`BaseUpgrade`]: each generation of behavior is a zero-sized type implementing [`AssetLogic`],
//! and [`asset_logic_for`] maps a fork to its generation. The version is selected once at precompile
//! construction and carried as the `L` type parameter on [`B20AssetToken`](crate::B20AssetToken),
//! so the hot path stays fully monomorphized — no vtable, no per-call fork branch.
//!
//! [`AssetLogic`] covers the **whole common (IB20) behavioral surface**, not just transfers. Every
//! method has a default that forwards to today's [`crate::common::ops`] implementation, so
//! [`AssetLogicV1`] is an empty impl (byte-identical to today) and a new generation overrides only
//! the methods it changes. Asset-specific extension methods (`batch_mint`, `update_multiplier`,
//! `announce`, ...) are inherent on [`B20AssetToken`](crate::B20AssetToken) rather than trait-backed;
//! versioning those is a follow-up that first puts them behind a capability trait.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_common_chains::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20Guards, B20PolicyType, Burnable, Configurable, IB20, Mintable, Pausable, PermitArgs,
    Permittable, RoleManaged, TokenAccounting, Transferable,
};

/// The capability surface an asset token exposes — the bound the versioned logic operates on.
///
/// These are the storage-backed behavior traits `B20AssetToken` already implements; `AssetLogic`
/// methods run against any `T` that provides them.
pub trait AssetOps:
    Transferable + Mintable + Burnable + Pausable + Configurable + Permittable + RoleManaged
{
}
impl<T> AssetOps for T where
    T: Transferable + Mintable + Burnable + Pausable + Configurable + Permittable + RoleManaged
{
}

/// A frozen generation of B-20 asset behavior.
///
/// Every method has a default that forwards to today's `common/ops` behavior, so implementing this
/// trait with an empty body ([`AssetLogicV1`]) reproduces current behavior exactly. A new generation
/// overrides only the methods it changes. Methods that share an internal helper (the transfer and
/// mint/burn clusters) must be overridden together so the shell keeps calling the new core — see
/// [`AssetLogicV2`].
///
/// Method docs are omitted (`allow(missing_docs)`): each method mirrors the identically-named,
/// already-documented method on the `common/ops` capability traits it forwards to.
#[allow(missing_docs)]
pub trait AssetLogic: Copy + 'static {
    // --- Transfer cluster ---
    fn transfer<T: AssetOps>(t: &mut T, from: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
        Transferable::transfer(t, from, to, amount, privileged)
    }
    fn transfer_inner<T: AssetOps>(t: &mut T, from: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
        Transferable::transfer_inner(t, from, to, amount, privileged)
    }
    fn transfer_from<T: AssetOps>(t: &mut T, spender: Address, from: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
        Transferable::transfer_from(t, spender, from, to, amount, privileged)
    }
    fn transfer_with_memo<T: AssetOps>(t: &mut T, from: Address, to: Address, amount: U256, memo: B256, privileged: bool) -> Result<()> {
        Transferable::transfer_with_memo(t, from, to, amount, memo, privileged)
    }
    fn transfer_from_with_memo<T: AssetOps>(t: &mut T, spender: Address, from: Address, to: Address, amount: U256, memo: B256, privileged: bool) -> Result<()> {
        Transferable::transfer_from_with_memo(t, spender, from, to, amount, memo, privileged)
    }
    fn approve<T: AssetOps>(t: &mut T, owner: Address, spender: Address, amount: U256) -> Result<()> {
        Transferable::approve(t, owner, spender, amount)
    }

    // --- Mint ---
    fn mint<T: AssetOps>(t: &mut T, caller: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
        Mintable::mint(t, caller, to, amount, privileged)
    }
    fn mint_with_memo<T: AssetOps>(t: &mut T, caller: Address, to: Address, amount: U256, memo: B256, privileged: bool) -> Result<()> {
        Mintable::mint_with_memo(t, caller, to, amount, memo, privileged)
    }

    // --- Burn ---
    fn burn<T: AssetOps>(t: &mut T, caller: Address, from: Address, amount: U256, privileged: bool) -> Result<()> {
        Burnable::burn(t, caller, from, amount, privileged)
    }
    fn burn_with_memo<T: AssetOps>(t: &mut T, caller: Address, from: Address, amount: U256, memo: B256, privileged: bool) -> Result<()> {
        Burnable::burn_with_memo(t, caller, from, amount, memo, privileged)
    }
    fn burn_blocked<T: AssetOps>(t: &mut T, caller: Address, from: Address, amount: U256, privileged: bool) -> Result<()> {
        Burnable::burn_blocked(t, caller, from, amount, privileged)
    }

    // --- Pause ---
    fn pause<T: AssetOps>(t: &mut T, caller: Address, features: Vec<IB20::PausableFeature>, privileged: bool) -> Result<()> {
        Pausable::pause(t, caller, features, privileged)
    }
    fn unpause<T: AssetOps>(t: &mut T, caller: Address, features: Vec<IB20::PausableFeature>, privileged: bool) -> Result<()> {
        Pausable::unpause(t, caller, features, privileged)
    }

    // --- Config ---
    fn update_supply_cap<T: AssetOps>(t: &mut T, caller: Address, new_cap: U256, privileged: bool) -> Result<()> {
        Configurable::update_supply_cap(t, caller, new_cap, privileged)
    }
    fn update_name<T: AssetOps>(t: &mut T, caller: Address, name: String, privileged: bool) -> Result<()> {
        Configurable::update_name(t, caller, name, privileged)
    }
    fn update_symbol<T: AssetOps>(t: &mut T, caller: Address, symbol: String, privileged: bool) -> Result<()> {
        Configurable::update_symbol(t, caller, symbol, privileged)
    }
    fn update_contract_uri<T: AssetOps>(t: &mut T, caller: Address, uri: String, privileged: bool) -> Result<()> {
        Configurable::update_contract_uri(t, caller, uri, privileged)
    }

    // --- Roles ---
    fn grant_role<T: AssetOps>(t: &mut T, caller: Address, role: B256, account: Address, privileged: bool) -> Result<()> {
        RoleManaged::grant_role(t, caller, role, account, privileged)
    }
    fn revoke_role<T: AssetOps>(t: &mut T, caller: Address, role: B256, account: Address, privileged: bool) -> Result<()> {
        RoleManaged::revoke_role(t, caller, role, account, privileged)
    }
    fn renounce_role<T: AssetOps>(t: &mut T, caller: Address, role: B256, confirmation: Address) -> Result<()> {
        RoleManaged::renounce_role(t, caller, role, confirmation)
    }
    fn renounce_last_admin<T: AssetOps>(t: &mut T, caller: Address) -> Result<()> {
        RoleManaged::renounce_last_admin(t, caller)
    }
    fn set_role_admin<T: AssetOps>(t: &mut T, caller: Address, role: B256, new_admin_role: B256, privileged: bool) -> Result<()> {
        RoleManaged::set_role_admin(t, caller, role, new_admin_role, privileged)
    }

    // --- Permit ---
    fn permit<T: AssetOps>(t: &mut T, chain_id: u64, now: U256, args: PermitArgs) -> Result<()> {
        Permittable::permit(t, chain_id, now, args)
    }
}

/// Genesis behavior (Beryl onward) — identical to today's `common/ops` implementation.
///
/// Empty impl: every method uses the forwarding default, so V1 is today's behavior by construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetLogicV1;

impl AssetLogic for AssetLogicV1 {}

/// Illustrative next-generation behavior: `transfer_inner` additionally rejects zero-amount
/// transfers with `InvalidAmount`. Every non-transfer method is inherited unchanged from V1.
///
/// The whole transfer cluster is restated so `Self::transfer_inner` binds to *this* generation:
/// `transfer` / `transfer_from` / the memo wrappers cannot inherit V1's forwarding defaults or their
/// internal calls would run V1's core — the "cluster rule". Only `transfer_inner` carries new logic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetLogicV2;

impl AssetLogic for AssetLogicV2 {
    fn transfer<T: AssetOps>(t: &mut T, from: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
        B20Guards::ensure_not_paused::<T>(t, IB20::PausableFeature::TRANSFER)?;
        Self::transfer_inner(t, from, to, amount, privileged)
    }

    fn transfer_inner<T: AssetOps>(t: &mut T, from: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
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

    fn transfer_from<T: AssetOps>(t: &mut T, spender: Address, from: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
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

    fn transfer_with_memo<T: AssetOps>(t: &mut T, from: Address, to: Address, amount: U256, memo: B256, privileged: bool) -> Result<()> {
        Self::transfer(t, from, to, amount, privileged)?;
        t.accounting_mut().emit_event(IB20::Memo { caller: from, memo }.encode_log_data())
    }

    fn transfer_from_with_memo<T: AssetOps>(t: &mut T, spender: Address, from: Address, to: Address, amount: U256, memo: B256, privileged: bool) -> Result<()> {
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
/// This is the single place a fork's behavior is bound — adding a fork's new behavior is a one-line
/// change here. For this sample, Cobalt (the latest fork) runs [`AssetLogicV2`] and every earlier
/// fork runs [`AssetLogicV1`], so the fork boundary is exercised through the real lookup/dispatch
/// path and not only by the unit tests below.
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
    fn v2_inherits_v1_behavior_for_untouched_methods() {
        // A non-transfer method (mint) is inherited from V1 via the forwarding default, so V1 and V2
        // behave identically for it — demonstrating "override only what changed".
        let mut v1_token = token_with_balance(U256::ZERO);
        let mut v2_token = token_with_balance(U256::ZERO);

        AssetLogicV1::mint(&mut v1_token, ALICE, BOB, U256::from(7u64), true).unwrap();
        AssetLogicV2::mint(&mut v2_token, ALICE, BOB, U256::from(7u64), true).unwrap();

        assert_eq!(
            v1_token.accounting().balance_of(BOB).unwrap(),
            v2_token.accounting().balance_of(BOB).unwrap(),
        );
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
