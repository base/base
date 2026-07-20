//! Append-only business-logic interface for the asset B-20 precompile.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256};
use base_precompile_storage::Result;

use crate::{
    AssetAccounting, ContractContext, Eip712Domain, IB20, PermitArgs, PolicyAccounting, Token,
};

/// The asset B-20 logic interface.
pub trait B20AssetLogic<S: AssetAccounting, A: PolicyAccounting> {
    /// ERC-20 `transfer`.
    fn transfer(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `transferFrom`.
    fn transfer_from(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `approve`.
    fn approve(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()>;

    /// Emits a `Memo` event attributed to `caller`.
    ///
    /// The memo-decorated ABI calls (`transferWithMemo`, `mintWithMemo`, …) are composed
    /// by the default `*_with_memo` methods below as the base operation followed by this
    /// event, so the memo semantics stay version-defined without widening every
    /// operation's signature or leaking composition into the dispatcher.
    fn emit_memo(&self, ctx: &mut ContractContext<S, A>, caller: Address, memo: B256)
    -> Result<()>;

    /// `transfer` followed by a `Memo` event (`transferWithMemo`).
    fn transfer_with_memo(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        call: IB20::transferWithMemoCall,
        privileged: bool,
    ) -> Result<()> {
        self.transfer(ctx, caller, call.to, call.amount, privileged)?;
        self.emit_memo(ctx, caller, call.memo)
    }

    /// `transferFrom` followed by a `Memo` event (`transferFromWithMemo`).
    fn transfer_from_with_memo(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        call: IB20::transferFromWithMemoCall,
        privileged: bool,
    ) -> Result<()> {
        self.transfer_from(ctx, caller, call.from, call.to, call.amount, privileged)?;
        self.emit_memo(ctx, caller, call.memo)
    }

    /// `mint` followed by a `Memo` event (`mintWithMemo`).
    fn mint_with_memo(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        call: IB20::mintWithMemoCall,
        privileged: bool,
    ) -> Result<()> {
        self.mint(ctx, caller, call.to, call.amount, privileged)?;
        self.emit_memo(ctx, caller, call.memo)
    }

    /// `burn` followed by a `Memo` event (`burnWithMemo`).
    fn burn_with_memo(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        call: IB20::burnWithMemoCall,
    ) -> Result<()> {
        self.burn(ctx, caller, call.amount)?;
        self.emit_memo(ctx, caller, call.memo)
    }

    /// Mints `amount` to `to`.
    fn mint(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Self-burn: the caller destroys `amount` of its own balance.
    fn burn(&self, ctx: &mut ContractContext<S, A>, caller: Address, amount: U256) -> Result<()>;

    /// Destroys `amount` from a policy-blocked `from` account.
    fn burn_blocked(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Pauses the given features.
    fn pause(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()>;

    /// Unpauses the given features.
    fn unpause(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the maximum total supply.
    fn update_supply_cap(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the token name.
    fn update_name(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the token symbol.
    fn update_symbol(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the contract URI.
    fn update_contract_uri(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()>;

    /// Grants `role` to `account`.
    fn grant_role(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()>;

    /// Grants `role` to `account` without checking caller authorization.
    ///
    /// The one token-level mutation the factory needs at bootstrap, when no admin exists yet and the
    /// authorized [`grant_role`](Self::grant_role) path is not reachable. Bumps the `DefaultAdmin`
    /// member count and emits `RoleGranted`.
    fn grant_role_unchecked(
        &self,
        ctx: &mut ContractContext<S, A>,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()>;

    /// Revokes `role` from `account`.
    fn revoke_role(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()>;

    /// Renounces `role` for the caller.
    fn renounce_role(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()>;

    /// Permanently removes the final default admin.
    fn renounce_last_admin(&self, ctx: &mut ContractContext<S, A>, caller: Address) -> Result<()>;

    /// Sets the admin role for `role`.
    fn set_role_admin(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the policy ID configured for `policy_scope`.
    fn update_policy(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()>;

    /// EIP-2612 `permit`.
    fn permit(
        &self,
        ctx: &mut ContractContext<S, A>,
        chain_id: u64,
        now: U256,
        args: PermitArgs,
    ) -> Result<()>;

    // --- Asset-specific mutations ---

    /// Sets a new multiplier. Requires the operator role unless privileged.
    fn update_multiplier(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        new_multiplier: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Sets, updates, or removes an extra-metadata entry. Requires the metadata role.
    fn update_extra_metadata(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        key: String,
        value: String,
        privileged: bool,
    ) -> Result<()>;

    /// Mints `amounts[i]` to `recipients[i]`. Requires the mint role. All-or-nothing.
    fn batch_mint(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()>;

    /// Opens an announcement: authorizes `caller`, consumes `id`, and emits `Announcement`.
    ///
    /// The dispatcher owns the atomic execution of the announcement's internal calls (routing is
    /// its responsibility); this method and [`Self::end_announce`] carry the version-defined
    /// business steps that bracket that loop.
    fn begin_announce(
        &self,
        ctx: &mut ContractContext<S, A>,
        caller: Address,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> Result<()>;

    /// Closes an announcement after its internal calls have executed: emits `EndAnnouncement`.
    fn end_announce(&self, ctx: &mut ContractContext<S, A>, id: String) -> Result<()>;

    // --- Direct reads: version-invariant pass-throughs to the storage port, so the
    //     dispatcher never touches token storage directly. Defaulted here and shared by
    //     every version; a version overrides one only if its read semantics change. ---

    /// Returns whether marker bytecode is deployed at this token's address.
    fn is_initialized(&self, ctx: &ContractContext<S, A>) -> Result<bool> {
        ctx.accounting().is_initialized()
    }

    /// Returns the token name.
    fn name(&self, ctx: &ContractContext<S, A>) -> Result<String> {
        ctx.accounting().name()
    }

    /// Returns the token symbol.
    fn symbol(&self, ctx: &ContractContext<S, A>) -> Result<String> {
        ctx.accounting().symbol()
    }

    /// Returns the custom decimal precision configured for this asset ctx.
    fn decimals(&self, ctx: &ContractContext<S, A>) -> Result<u8> {
        AssetAccounting::decimals(ctx.accounting())
    }

    /// Returns the total token supply currently in circulation.
    fn total_supply(&self, ctx: &ContractContext<S, A>) -> Result<U256> {
        ctx.accounting().total_supply()
    }

    /// Returns the token balance of `account`.
    fn balance_of(&self, ctx: &ContractContext<S, A>, account: Address) -> Result<U256> {
        ctx.accounting().balance_of(account)
    }

    /// Returns the allowance granted by `owner` to `spender`.
    fn allowance(
        &self,
        ctx: &ContractContext<S, A>,
        owner: Address,
        spender: Address,
    ) -> Result<U256> {
        ctx.accounting().allowance(owner, spender)
    }

    /// Returns the maximum total supply enforced on mint.
    fn supply_cap(&self, ctx: &ContractContext<S, A>) -> Result<U256> {
        ctx.accounting().supply_cap()
    }

    /// Returns the current EIP-2612 permit nonce for `owner`.
    fn nonce(&self, ctx: &ContractContext<S, A>, owner: Address) -> Result<U256> {
        ctx.accounting().nonce(owner)
    }

    /// Returns the off-chain metadata URI for this token (ERC-7572).
    fn contract_uri(&self, ctx: &ContractContext<S, A>) -> Result<String> {
        ctx.accounting().contract_uri()
    }

    /// Returns whether `account` has `role`.
    fn has_role(&self, ctx: &ContractContext<S, A>, role: B256, account: Address) -> Result<bool> {
        ctx.accounting().has_role(role, account)
    }

    /// Returns the admin role for `role`.
    fn role_admin(&self, ctx: &ContractContext<S, A>, role: B256) -> Result<B256> {
        ctx.accounting().role_admin(role)
    }

    /// Returns the current multiplier, scaled to WAD.
    fn multiplier(&self, ctx: &ContractContext<S, A>) -> Result<U256> {
        ctx.accounting().multiplier()
    }

    /// Returns the extra-metadata value for `key`, or the empty string if unset.
    fn extra_metadata(&self, ctx: &ContractContext<S, A>, key: &str) -> Result<String> {
        ctx.accounting().extra_metadata(key)
    }

    /// Returns whether announcement `id` has already been consumed.
    fn is_announcement_id_used(&self, ctx: &ContractContext<S, A>, id: &str) -> Result<bool> {
        ctx.accounting().is_announcement_id_used(id)
    }

    // --- Computed reads: derive from storage but encode version-defined semantics ---

    /// Returns whether the given pause `feature` is currently set.
    fn is_paused(
        &self,
        ctx: &ContractContext<S, A>,
        feature: IB20::PausableFeature,
    ) -> Result<bool>;

    /// Returns all currently paused features.
    fn paused_features(&self, ctx: &ContractContext<S, A>) -> Result<Vec<IB20::PausableFeature>>;

    /// Returns the configured policy ID for `policy_scope`, validating the scope.
    fn policy_id(&self, ctx: &ContractContext<S, A>, policy_scope: B256) -> Result<u64>;

    /// Computes the EIP-712 domain separator for this ctx.
    fn domain_separator(&self, ctx: &ContractContext<S, A>, chain_id: u64) -> Result<B256>;

    /// Returns the ERC-5267 `eip712Domain()` tuple for this ctx.
    fn eip712_domain(&self, ctx: &ContractContext<S, A>, chain_id: u64) -> Result<Eip712Domain>;

    /// Converts a raw balance to its scaled view: `rawBalance * multiplier / WAD`.
    fn to_scaled_balance(&self, ctx: &ContractContext<S, A>, balance: U256) -> Result<U256>;

    /// Converts a scaled balance back to its raw representation: `scaledBalance * WAD / multiplier`.
    fn to_raw_balance(&self, ctx: &ContractContext<S, A>, balance: U256) -> Result<U256>;

    /// Returns the scaled balance for `account`.
    fn scaled_balance_of(&self, ctx: &ContractContext<S, A>, account: Address) -> Result<U256>;

    /// Returns the asset operator role identifier (required for `announce` / `updateMultiplier`).
    fn operator_role(&self) -> B256;
}
