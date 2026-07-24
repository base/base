//! Version 2 scaffolding for the asset B-20 precompile, activated at Cobalt.
//!
//! This version initially delegates existing behavior to the frozen [`AssetV1`].
//! ERC-8056 behavior is introduced separately by overriding the interface defaults.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256};
use base_precompile_storage::Result;

use crate::{
    Asset, AssetAccounting, AssetV1, B20AssetToken, Eip712Domain, IB20, PermitArgs,
    PolicyAccounting,
};

/// Second B-20 Asset precompile implementation, introduced at Cobalt.
#[derive(Debug, Default, Clone, Copy)]
pub struct AssetV2;

impl<S: AssetAccounting, A: PolicyAccounting> Asset<S, A> for AssetV2 {
    fn transfer(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.transfer(token, caller, to, amount, privileged)
    }

    fn transfer_from(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.transfer_from(token, caller, from, to, amount, privileged)
    }

    fn approve(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()> {
        AssetV1.approve(token, caller, spender, amount)
    }

    fn emit_memo(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        memo: B256,
    ) -> Result<()> {
        AssetV1.emit_memo(token, caller, memo)
    }

    fn mint(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.mint(token, caller, to, amount, privileged)
    }

    fn burn(&self, token: &mut B20AssetToken<S, A>, caller: Address, amount: U256) -> Result<()> {
        AssetV1.burn(token, caller, amount)
    }

    fn burn_blocked(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.burn_blocked(token, caller, from, amount, privileged)
    }

    fn pause(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.pause(token, caller, features, privileged)
    }

    fn unpause(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.unpause(token, caller, features, privileged)
    }

    fn update_supply_cap(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_supply_cap(token, caller, new_cap, privileged)
    }

    fn update_name(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_name(token, caller, name, privileged)
    }

    fn update_symbol(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_symbol(token, caller, symbol, privileged)
    }

    fn update_contract_uri(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_contract_uri(token, caller, uri, privileged)
    }

    fn grant_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.grant_role(token, caller, role, account, privileged)
    }

    fn revoke_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.revoke_role(token, caller, role, account, privileged)
    }

    fn renounce_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()> {
        AssetV1.renounce_role(token, caller, role, confirmation)
    }

    fn renounce_last_admin(&self, token: &mut B20AssetToken<S, A>, caller: Address) -> Result<()> {
        AssetV1.renounce_last_admin(token, caller)
    }

    fn set_role_admin(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.set_role_admin(token, caller, role, new_admin_role, privileged)
    }

    fn update_policy(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_policy(token, caller, policy_scope, new_policy_id, privileged)
    }

    fn permit(
        &self,
        token: &mut B20AssetToken<S, A>,
        chain_id: u64,
        now: U256,
        args: PermitArgs,
    ) -> Result<()> {
        AssetV1.permit(token, chain_id, now, args)
    }

    fn update_multiplier(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_multiplier: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_multiplier(token, caller, new_multiplier, privileged)
    }

    fn update_extra_metadata(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        key: String,
        value: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_extra_metadata(token, caller, key, value, privileged)
    }

    fn batch_mint(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.batch_mint(token, caller, recipients, amounts, privileged)
    }

    fn begin_announce(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.begin_announce(token, caller, id, description, uri, privileged)
    }

    fn end_announce(&self, token: &mut B20AssetToken<S, A>, id: String) -> Result<()> {
        AssetV1.end_announce(token, id)
    }

    fn is_paused(
        &self,
        token: &B20AssetToken<S, A>,
        feature: IB20::PausableFeature,
    ) -> Result<bool> {
        AssetV1.is_paused(token, feature)
    }

    fn paused_features(&self, token: &B20AssetToken<S, A>) -> Result<Vec<IB20::PausableFeature>> {
        AssetV1.paused_features(token)
    }

    fn policy_id(&self, token: &B20AssetToken<S, A>, policy_scope: B256) -> Result<u64> {
        AssetV1.policy_id(token, policy_scope)
    }

    fn domain_separator(&self, token: &B20AssetToken<S, A>, chain_id: u64) -> Result<B256> {
        AssetV1.domain_separator(token, chain_id)
    }

    fn eip712_domain(&self, token: &B20AssetToken<S, A>, chain_id: u64) -> Result<Eip712Domain> {
        AssetV1.eip712_domain(token, chain_id)
    }

    fn to_scaled_balance(&self, token: &B20AssetToken<S, A>, balance: U256) -> Result<U256> {
        AssetV1.to_scaled_balance(token, balance)
    }

    fn to_raw_balance(&self, token: &B20AssetToken<S, A>, balance: U256) -> Result<U256> {
        AssetV1.to_raw_balance(token, balance)
    }

    fn scaled_balance_of(&self, token: &B20AssetToken<S, A>, account: Address) -> Result<U256> {
        AssetV1.scaled_balance_of(token, account)
    }

    fn operator_role(&self) -> B256 {
        AssetV1::OPERATOR_ROLE
    }
}
