//! Version 1 of the stablecoin B-20 precompile logic, activated at Beryl.
//!
//! `StablecoinV1` is the first, frozen implementation of [`StablecoinLogic`]. It
//! is fully self-contained: every operation's orchestration (balance and
//! allowance math, supply-cap and guard ordering, role bookkeeping, EIP-712
//! permit verification, event emission) is implemented here directly against the
//! storage port (`token.accounting()`/`accounting_mut()`/`policy()`). It reuses
//! only stateless shared primitives — [`B20Guards`] checks, [`B20PausableFeature`],
//! [`B20PolicyType`], [`B20TokenRole`], [`PermitArgs`] EIP-712 crypto, and the
//! `IB20` ABI types — and never calls the token's capability-trait methods.
//!
//! Once activated it is immutable: new behavior must be introduced through a new
//! version, not by editing this file.

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use alloy_primitives::{Address, B256, FixedBytes, U256, b256, keccak256};
use alloy_sol_types::{SolEvent, SolValue};
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20Guards, B20PausableFeature, B20PolicyType, B20StablecoinToken, B20TokenRole,
    B20_MAX_SUPPLY_CAP, Eip712Domain, IB20, PermitArgs, Policy, StablecoinAccounting,
    StablecoinLogic, Token,
};

/// `keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")`
const DOMAIN_TYPEHASH: B256 =
    b256!("8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f");

/// EIP-712 domain version string pinned to `"1"`.
const VERSION: &[u8] = b"1";

/// First stablecoin B-20 implementation. Frozen as of its activation at Beryl.
#[derive(Debug, Default, Clone, Copy)]
pub struct StablecoinV1;

impl StablecoinV1 {
    /// Balance-moving core of `transfer`/`transferFrom`, without the pause check.
    fn transfer_inner<S: StablecoinAccounting, P: Policy>(
        &self,
        token: &mut B20StablecoinToken<S, P>,
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
            B20Guards::ensure_policy_type(token, B20PolicyType::TransferSender, from)?;
            B20Guards::ensure_policy_type(token, B20PolicyType::TransferReceiver, to)?;
        }
        let from_balance = token.accounting().balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance: from_balance,
                needed: amount,
            }));
        }
        let new_from_balance =
            from_balance.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting_mut().set_balance(from, new_from_balance)?;
        let to_balance = token.accounting().balance_of(to)?;
        let new_to_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting_mut().set_balance(to, new_to_balance)?;
        token.accounting_mut().emit_event(IB20::Transfer { from, to, amount }.encode_log_data())
    }

    /// Supply-reducing core of the burn operations, without pause or role checks.
    fn burn_inner<S: StablecoinAccounting, P: Policy>(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        from: Address,
        amount: U256,
    ) -> Result<()> {
        let balance = token.accounting().balance_of(from)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance,
                needed: amount,
            }));
        }
        token.accounting_mut().set_balance(from, balance - amount)?;
        let supply = token.accounting().total_supply()?;
        let new_supply =
            supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting_mut().set_total_supply(new_supply)?;
        token
            .accounting_mut()
            .emit_event(IB20::Transfer { from, to: Address::ZERO, amount }.encode_log_data())
    }

    /// Revokes `role` from `account` without checking caller authorization.
    fn revoke_role_unchecked<S: StablecoinAccounting, P: Policy>(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if !token.accounting().has_role(role, account)? {
            return Ok(());
        }
        token.accounting_mut().set_role(role, account, false)?;
        if role == B20TokenRole::DefaultAdmin.id() {
            let current = token.accounting().role_member_count(role)?;
            let next =
                current.checked_sub(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
            token.accounting_mut().set_role_member_count(role, next)?;
        }
        token
            .accounting_mut()
            .emit_event(IB20::RoleRevoked { role, account, sender }.encode_log_data())
    }

    /// Ensures role-admin mutations are still reachable.
    fn ensure_role_admin_mutations_available<S: StablecoinAccounting, P: Policy>(
        &self,
        token: &B20StablecoinToken<S, P>,
        caller: Address,
    ) -> Result<()> {
        let admin_role = B20TokenRole::DefaultAdmin.id();
        if token.accounting().role_member_count(admin_role)? == U256::ZERO {
            return Err(BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: caller,
                neededRole: admin_role,
            }));
        }
        Ok(())
    }

    /// Ensures `policy_scope` names a built-in B-20 policy slot.
    fn ensure_supported_policy_type(policy_scope: B256) -> Result<()> {
        if B20PolicyType::from_id(policy_scope).is_some() {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }))
        }
    }
}

impl<S: StablecoinAccounting, P: Policy> StablecoinLogic<S, P> for StablecoinV1 {
    fn transfer(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused(token, IB20::PausableFeature::TRANSFER)?;
        self.transfer_inner(token, caller, to, amount, privileged)
    }

    fn transfer_from(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused(token, IB20::PausableFeature::TRANSFER)?;
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        let allowance = token.accounting().allowance(from, caller)?;
        let is_infinite = allowance == U256::MAX;
        if !is_infinite && allowance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender: caller,
                allowance,
                needed: amount,
            }));
        }
        if !privileged && caller != from {
            B20Guards::ensure_policy_type(token, B20PolicyType::TransferExecutor, caller)?;
        }
        self.transfer_inner(token, from, to, amount, privileged)?;
        if is_infinite {
            return Ok(());
        }
        token.accounting_mut().set_allowance(from, caller, allowance - amount)
    }

    fn approve(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()> {
        if caller == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidApprover { approver: caller }));
        }
        if spender == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSpender { spender }));
        }
        token.accounting_mut().set_allowance(caller, spender, amount)?;
        token
            .accounting_mut()
            .emit_event(IB20::Approval { owner: caller, spender, amount }.encode_log_data())
    }

    fn emit_memo(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        memo: B256,
    ) -> Result<()> {
        token.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())
    }

    fn mint(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused(token, IB20::PausableFeature::MINT)?;
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::Mint)?;
        }
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        B20Guards::ensure_policy_type(token, B20PolicyType::MintReceiver, to)?;
        let supply = token.accounting().total_supply()?;
        let cap = token.accounting().supply_cap()?;
        let new_supply =
            supply.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        if new_supply > cap {
            return Err(BasePrecompileError::revert(IB20::SupplyCapExceeded {
                cap,
                attempted: new_supply,
            }));
        }
        token.accounting_mut().set_total_supply(new_supply)?;
        let to_balance = token.accounting().balance_of(to)?;
        let new_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting_mut().set_balance(to, new_balance)?;
        token
            .accounting_mut()
            .emit_event(IB20::Transfer { from: Address::ZERO, to, amount }.encode_log_data())
    }

    fn burn(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        amount: U256,
    ) -> Result<()> {
        // Self-burn: `from == caller`, never factory-privileged.
        B20Guards::ensure_not_paused(token, IB20::PausableFeature::BURN)?;
        B20Guards::ensure_token_role(token, caller, B20TokenRole::Burn)?;
        self.burn_inner(token, caller, amount)
    }

    fn burn_blocked(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        B20Guards::ensure_not_paused(token, IB20::PausableFeature::BURN)?;
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::BurnBlocked)?;
        }
        B20Guards::ensure_blocked(token, from)?;
        self.burn_inner(token, from, amount)?;
        token
            .accounting_mut()
            .emit_event(IB20::BurnedBlocked { caller, from, amount }.encode_log_data())
    }

    fn pause(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        for feature in &features {
            B20PausableFeature::ensure_valid(*feature)?;
        }
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::Pause)?;
        }
        if features.is_empty() {
            return Err(BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
        }
        let mut next = token.accounting().paused()?;
        for feature in &features {
            next |= B20PausableFeature::mask(*feature);
        }
        token.accounting_mut().set_paused(next)?;
        token
            .accounting_mut()
            .emit_event(IB20::Paused { updater: caller, features }.encode_log_data())
    }

    fn unpause(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        for feature in &features {
            B20PausableFeature::ensure_valid(*feature)?;
        }
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::Unpause)?;
        }
        if features.is_empty() {
            return Err(BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
        }
        let mut next = token.accounting().paused()?;
        for feature in &features {
            next &= !B20PausableFeature::mask(*feature);
        }
        token.accounting_mut().set_paused(next)?;
        token
            .accounting_mut()
            .emit_event(IB20::Unpaused { updater: caller, features }.encode_log_data())
    }

    fn update_supply_cap(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::DefaultAdmin)?;
        }
        let supply = token.accounting().total_supply()?;
        if new_cap < supply || new_cap > B20_MAX_SUPPLY_CAP {
            return Err(BasePrecompileError::revert(IB20::InvalidSupplyCap {
                currentSupply: supply,
                proposedCap: new_cap,
            }));
        }
        let old = token.accounting().supply_cap()?;
        token.accounting_mut().set_supply_cap(new_cap)?;
        token.accounting_mut().emit_event(
            IB20::SupplyCapUpdated { updater: caller, oldSupplyCap: old, newSupplyCap: new_cap }
                .encode_log_data(),
        )
    }

    fn update_name(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::Metadata)?;
        }
        token.accounting_mut().set_name(name.clone())?;
        token
            .accounting_mut()
            .emit_event(IB20::NameUpdated { updater: caller, newName: name }.encode_log_data())?;
        token.accounting_mut().emit_event(IB20::EIP712DomainChanged {}.encode_log_data())
    }

    fn update_symbol(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::Metadata)?;
        }
        token.accounting_mut().set_symbol(symbol.clone())?;
        token.accounting_mut().emit_event(
            IB20::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data(),
        )
    }

    fn update_contract_uri(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::Metadata)?;
        }
        token.accounting_mut().set_contract_uri(uri)?;
        token.accounting_mut().emit_event(IB20::ContractURIUpdated {}.encode_log_data())
    }

    fn grant_role(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        if role == B20TokenRole::DefaultAdmin.id() || !privileged {
            self.ensure_role_admin_mutations_available(token, caller)?;
        }
        if !privileged {
            let admin = token.accounting().role_admin(role)?;
            B20Guards::ensure_role(token, caller, admin)?;
        }
        self.grant_role_unchecked(token, role, account, caller)
    }

    fn revoke_role(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            self.ensure_role_admin_mutations_available(token, caller)?;
            let admin = token.accounting().role_admin(role)?;
            B20Guards::ensure_role(token, caller, admin)?;
        }
        if role == B20TokenRole::DefaultAdmin.id()
            && token.accounting().has_role(role, account)?
            && token.accounting().role_member_count(role)? == U256::ONE
        {
            return Err(BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
        }
        self.revoke_role_unchecked(token, role, account, caller)
    }

    fn renounce_role(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()> {
        if confirmation != caller {
            return Err(BasePrecompileError::revert(IB20::AccessControlBadConfirmation {}));
        }
        if role == B20TokenRole::DefaultAdmin.id()
            && token.accounting().has_role(role, caller)?
            && token.accounting().role_member_count(role)? == U256::ONE
        {
            return Err(BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
        }
        self.revoke_role_unchecked(token, role, caller, caller)
    }

    fn renounce_last_admin(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
    ) -> Result<()> {
        let admin_role = B20TokenRole::DefaultAdmin.id();
        B20Guards::ensure_role(token, caller, admin_role)?;
        if token.accounting().role_member_count(admin_role)? != U256::ONE {
            return Err(BasePrecompileError::revert(IB20::NotSoleAdmin {}));
        }
        self.revoke_role_unchecked(token, admin_role, caller, caller)?;
        token
            .accounting_mut()
            .emit_event(IB20::LastAdminRenounced { previousAdmin: caller }.encode_log_data())
    }

    fn set_role_admin(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()> {
        let previous_admin_role = token.accounting().role_admin(role)?;
        if !privileged {
            self.ensure_role_admin_mutations_available(token, caller)?;
            B20Guards::ensure_role(token, caller, previous_admin_role)?;
        }
        token.accounting_mut().set_role_admin(role, new_admin_role)?;
        token.accounting_mut().emit_event(
            IB20::RoleAdminChanged {
                role,
                previousAdminRole: previous_admin_role,
                newAdminRole: new_admin_role,
            }
            .encode_log_data(),
        )
    }

    fn update_policy(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::DefaultAdmin)?;
        }
        let old_policy_id = self.policy_id(token, policy_scope)?;
        if !token.policy().policy_exists(new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        token.accounting_mut().set_policy_id(policy_scope, new_policy_id)?;
        token.accounting_mut().emit_event(
            IB20::PolicyUpdated {
                policyScope: policy_scope,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }

    fn permit(
        &self,
        token: &mut B20StablecoinToken<S, P>,
        chain_id: u64,
        now: U256,
        args: PermitArgs,
    ) -> Result<()> {
        if now > args.deadline {
            return Err(BasePrecompileError::revert(IB20::ExpiredSignature {
                deadline: args.deadline,
            }));
        }
        let domain_sep = self.domain_separator(token, chain_id)?;
        let nonce = token.accounting().nonce(args.owner)?;
        let signing_hash = args.signing_hash(domain_sep, nonce);
        let recovered = args.recover_signer(signing_hash)?;
        PermitArgs::validate_recovered_address(recovered, args.owner)?;
        token.accounting_mut().increment_nonce(args.owner)?;
        self.approve(token, args.owner, args.spender, args.value)
    }

    fn is_paused(
        &self,
        token: &B20StablecoinToken<S, P>,
        feature: IB20::PausableFeature,
    ) -> Result<bool> {
        B20PausableFeature::ensure_valid(feature)?;
        Ok((token.accounting().paused()? & B20PausableFeature::mask(feature)) != U256::ZERO)
    }

    fn paused_features(
        &self,
        token: &B20StablecoinToken<S, P>,
    ) -> Result<Vec<IB20::PausableFeature>> {
        let paused = token.accounting().paused()?;
        let mut features = Vec::new();
        for feature in [
            IB20::PausableFeature::TRANSFER,
            IB20::PausableFeature::MINT,
            IB20::PausableFeature::BURN,
        ] {
            if (paused & B20PausableFeature::mask(feature)) != U256::ZERO {
                features.push(feature);
            }
        }
        Ok(features)
    }

    fn policy_id(&self, token: &B20StablecoinToken<S, P>, policy_scope: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_scope)?;
        token.accounting().policy_id(policy_scope)
    }

    fn domain_separator(&self, token: &B20StablecoinToken<S, P>, chain_id: u64) -> Result<B256> {
        let name = token.accounting().name()?;
        let name_hash = keccak256(name.as_bytes());
        let version_hash = keccak256(VERSION);
        let encoded = (
            DOMAIN_TYPEHASH,
            name_hash,
            version_hash,
            U256::from(chain_id),
            token.token_address(),
        )
            .abi_encode();
        Ok(keccak256(&encoded))
    }

    fn eip712_domain(
        &self,
        token: &B20StablecoinToken<S, P>,
        chain_id: u64,
    ) -> Result<Eip712Domain> {
        let name = token.accounting().name()?;
        Ok((
            // bits 0+1+2+3: name + version + chainId + verifyingContract
            FixedBytes::<1>::from([0x0f]),
            name,
            "1".to_string(),
            U256::from(chain_id),
            token.token_address(),
            B256::ZERO,
            vec![],
        ))
    }

    fn currency(&self, token: &B20StablecoinToken<S, P>) -> Result<String> {
        token.accounting().currency()
    }
}
