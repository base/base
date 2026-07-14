//! Append-only business-logic interface for the stablecoin B-20 precompile.
//!
//! [`StablecoinLogic`] defines the mutating operations and computed reads the
//! stablecoin supports, expressed in domain terms (addresses, amounts, roles)
//! rather than ABI encodings. The dispatcher owns selector decoding and result
//! encoding; each version owns only the business rules.
//!
//! Each version is a distinct type implementing this interface (see
//! [`crate::StablecoinV1`]). Once a version is activated at a hardfork it is
//! frozen (open/closed principle): new behavior is introduced by a new version,
//! and new operations are added as new trait methods so existing versions stay
//! unchanged.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20StablecoinToken, B20TokenRole, Eip712Domain, IB20, PermitArgs, Policy, StablecoinAccounting,
    Token,
};

/// Append-only business-logic interface shared by every stablecoin version.
///
/// Methods operate directly on the token's storage and policy layer via the
/// supplied [`B20StablecoinToken`]. The trait is parameterized by the storage
/// (`S`) and policy (`P`) adapters so tests can inject in-memory backends; the
/// production path uses the EVM-backed storage.
pub trait StablecoinLogic<S: StablecoinAccounting, P: Policy> {
    /// ERC-20 `transfer`.
    fn transfer(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `transferFrom`.
    fn transfer_from(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `approve`.
    fn approve(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()>;

    /// Emits a `Memo` event attributed to `caller`.
    ///
    /// The memo-decorated ABI calls (`transferWithMemo`, `mintWithMemo`, …) are composed
    /// by the dispatcher as the base operation followed by this event, so the memo semantics
    /// stay version-defined without widening every operation's signature.
    fn emit_memo(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        memo: B256,
    ) -> Result<()>;

    /// Mints `amount` to `to`.
    fn mint(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Self-burn: the caller destroys `amount` of its own balance.
    fn burn(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        amount: U256,
    ) -> Result<()>;

    /// Destroys `amount` from a policy-blocked `from` account.
    fn burn_blocked(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Pauses the given features.
    fn pause(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()>;

    /// Unpauses the given features.
    fn unpause(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the maximum total supply.
    fn update_supply_cap(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the token name.
    fn update_name(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the token symbol.
    fn update_symbol(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the contract URI.
    fn update_contract_uri(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()>;

    /// Grants `role` to `account`.
    fn grant_role(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()>;

    /// Revokes `role` from `account`.
    fn revoke_role(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()>;

    /// Renounces `role` for the caller.
    fn renounce_role(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()>;

    /// Permanently removes the final default admin.
    fn renounce_last_admin(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
    ) -> Result<()>;

    /// Sets the admin role for `role`.
    fn set_role_admin(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the policy ID configured for `policy_scope`.
    fn update_policy(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()>;

    /// EIP-2612 `permit`.
    fn permit(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        chain_id: u64,
        now: U256,
        args: PermitArgs,
    ) -> Result<()>;

    // --- Direct reads: version-invariant pass-throughs to the storage port, so the
    //     dispatcher never touches token storage directly. Defaulted here and shared by
    //     every version; a version overrides one only if its read semantics change. ---

    /// Returns whether marker bytecode is deployed at this token's address.
    fn is_initialized(&self, token: &B20StablecoinToken<S, P>) -> Result<bool> {
        token.accounting().is_initialized()
    }

    /// Returns the token name.
    fn name(&self, token: &B20StablecoinToken<S, P>) -> Result<String> {
        token.accounting().name()
    }

    /// Returns the token symbol.
    fn symbol(&self, token: &B20StablecoinToken<S, P>) -> Result<String> {
        token.accounting().symbol()
    }

    /// Returns the total token supply currently in circulation.
    fn total_supply(&self, token: &B20StablecoinToken<S, P>) -> Result<U256> {
        token.accounting().total_supply()
    }

    /// Returns the token balance of `account`.
    fn balance_of(&self, token: &B20StablecoinToken<S, P>, account: Address) -> Result<U256> {
        token.accounting().balance_of(account)
    }

    /// Returns the allowance granted by `owner` to `spender`.
    fn allowance(
        &self,
        token: &B20StablecoinToken<S, P>,
        owner: Address,
        spender: Address,
    ) -> Result<U256> {
        token.accounting().allowance(owner, spender)
    }

    /// Returns the maximum total supply enforced on mint.
    fn supply_cap(&self, token: &B20StablecoinToken<S, P>) -> Result<U256> {
        token.accounting().supply_cap()
    }

    /// Returns the current EIP-2612 permit nonce for `owner`.
    fn nonce(&self, token: &B20StablecoinToken<S, P>, owner: Address) -> Result<U256> {
        token.accounting().nonce(owner)
    }

    /// Returns the off-chain metadata URI for this token (ERC-7572).
    fn contract_uri(&self, token: &B20StablecoinToken<S, P>) -> Result<String> {
        token.accounting().contract_uri()
    }

    /// Returns whether `account` has `role`.
    fn has_role(
        &self,
        token: &B20StablecoinToken<S, P>,
        role: B256,
        account: Address,
    ) -> Result<bool> {
        token.accounting().has_role(role, account)
    }

    /// Returns the admin role for `role`.
    fn role_admin(&self, token: &B20StablecoinToken<S, P>, role: B256) -> Result<B256> {
        token.accounting().role_admin(role)
    }

    /// Grants `role` to `account` without checking caller authorization.
    ///
    /// The one token-level mutation the factory needs at bootstrap, when no admin exists yet and
    /// the authorized [`grant_role`](Self::grant_role) path is not reachable. Bumps the
    /// `DefaultAdmin` member count and emits `RoleGranted`.
    fn grant_role_unchecked(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if token.accounting().has_role(role, account)? {
            return Ok(());
        }
        token.accounting_mut().set_role(role, account, true)?;
        if role == B20TokenRole::DefaultAdmin.id() {
            let current = token.accounting().role_member_count(role)?;
            let next =
                current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
            token.accounting_mut().set_role_member_count(role, next)?;
        }
        token
            .accounting_mut()
            .emit_event(IB20::RoleGranted { role, account, sender }.encode_log_data())
    }

    // --- Computed reads: derive from storage but encode version-defined semantics ---

    /// Returns whether the given pause `feature` is currently set.
    fn is_paused(
        &self,
        token: &B20StablecoinToken<S, P>,
        feature: IB20::PausableFeature,
    ) -> Result<bool>;

    /// Returns all currently paused features.
    fn paused_features(
        &self,
        token: &B20StablecoinToken<S, P>,
    ) -> Result<Vec<IB20::PausableFeature>>;

    /// Returns the configured policy ID for `policy_scope`, validating the scope.
    fn policy_id(&self, token: &B20StablecoinToken<S, P>, policy_scope: B256) -> Result<u64>;

    /// Computes the EIP-712 domain separator for this token.
    fn domain_separator(&self, token: &B20StablecoinToken<S, P>, chain_id: u64) -> Result<B256>;

    /// Returns the ERC-5267 `eip712Domain()` tuple for this token.
    fn eip712_domain(
        &self,
        token: &B20StablecoinToken<S, P>,
        chain_id: u64,
    ) -> Result<Eip712Domain>;

    /// Returns the stablecoin currency identifier — the stablecoin-specific
    /// extension operation.
    fn currency(&self, token: &B20StablecoinToken<S, P>) -> Result<String>;
}
