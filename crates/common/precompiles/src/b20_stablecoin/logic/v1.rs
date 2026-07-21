//! Version 1 of the stablecoin B-20 precompile logic, activated at Beryl.

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use alloy_primitives::{Address, B256, FixedBytes, U256, b256, keccak256};
use alloy_sol_types::{SolEvent, SolValue};
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20_MAX_SUPPLY_CAP, B20Guards, B20PausableFeature, B20PolicyType, B20StablecoinToken,
    B20TokenRole, Eip712Domain, IB20, PermitArgs, PolicyAccounting, Stablecoin,
    StablecoinAccounting, Token,
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
    fn transfer_inner<S: StablecoinAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20StablecoinToken<S, A>,
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
    fn burn_inner<S: StablecoinAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20StablecoinToken<S, A>,
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

    /// Grants `role` to `account` without checking caller authorization.
    ///
    /// The one token-level mutation the factory needs at bootstrap, when no admin exists yet and the
    /// authorized [`grant_role`](Stablecoin::grant_role) path is not reachable. Bumps the
    /// `DefaultAdmin` member count and emits `RoleGranted`. Kept inherent to V1 (off the `Stablecoin`
    /// trait) so it stays frozen with this version and off `&dyn Stablecoin`.
    pub(crate) fn grant_role_unchecked<S: StablecoinAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20StablecoinToken<S, A>,
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

    /// Revokes `role` from `account` without checking caller authorization.
    fn revoke_role_unchecked<S: StablecoinAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20StablecoinToken<S, A>,
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
    fn ensure_role_admin_mutations_available<S: StablecoinAccounting, A: PolicyAccounting>(
        &self,
        token: &B20StablecoinToken<S, A>,
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

impl<S: StablecoinAccounting, A: PolicyAccounting> Stablecoin<S, A> for StablecoinV1 {
    fn transfer(
        &self,
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
        caller: Address,
        memo: B256,
    ) -> Result<()> {
        token.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())
    }

    fn mint(
        &self,
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &mut B20StablecoinToken<S, A>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(token, caller, B20TokenRole::DefaultAdmin)?;
        }
        let old_policy_id = self.policy_id(token, policy_scope)?;
        if !token.policy().policy_exists(token.policy_storage(), new_policy_id)? {
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
        token: &mut B20StablecoinToken<S, A>,
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
        token: &B20StablecoinToken<S, A>,
        feature: IB20::PausableFeature,
    ) -> Result<bool> {
        B20PausableFeature::ensure_valid(feature)?;
        Ok((token.accounting().paused()? & B20PausableFeature::mask(feature)) != U256::ZERO)
    }

    fn paused_features(
        &self,
        token: &B20StablecoinToken<S, A>,
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

    fn policy_id(&self, token: &B20StablecoinToken<S, A>, policy_scope: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_scope)?;
        token.accounting().policy_id(policy_scope)
    }

    fn domain_separator(&self, token: &B20StablecoinToken<S, A>, chain_id: u64) -> Result<B256> {
        let name = token.accounting().name()?;
        let name_hash = keccak256(name.as_bytes());
        let version_hash = keccak256(VERSION);
        let encoded =
            (DOMAIN_TYPEHASH, name_hash, version_hash, U256::from(chain_id), token.token_address())
                .abi_encode();
        Ok(keccak256(&encoded))
    }

    fn eip712_domain(
        &self,
        token: &B20StablecoinToken<S, A>,
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

    fn currency(&self, token: &B20StablecoinToken<S, A>) -> Result<String> {
        token.accounting().currency()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        collections::BTreeMap,
        string::{String, ToString},
        vec,
        vec::Vec,
    };

    use alloy_primitives::{Address, B256, LogData, U256, keccak256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{BasePrecompileError, Result};
    use k256::ecdsa::SigningKey;

    use crate::{
        B20_MAX_SUPPLY_CAP, B20PolicyType, B20StablecoinToken, B20TokenRole, IB20, PackedPolicy,
        PermitArgs, PolicyAccounting, PolicyRegistryStorage, PolicyVersion, Stablecoin,
        StablecoinAccounting, StablecoinV1, Token, TokenAccounting,
    };

    // --- Self-contained in-memory fakes (no dependency on `common::test_utils`, so shared test
    //     scaffolding can never drift this frozen version's coverage) ---

    const TOKEN: Address = Address::repeat_byte(0x22);
    const ADMIN: Address = Address::repeat_byte(0xAD);
    const ALICE: Address = Address::repeat_byte(0xA1);
    const BOB: Address = Address::repeat_byte(0xB0);
    const CHAIN_ID: u64 = 8453;
    const LOGIC: StablecoinV1 = StablecoinV1;

    // Anvil/Hardhat account 0 — well-known test key, never used in production.
    const PRIVATE_KEY: [u8; 32] =
        alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    /// Minimal `StablecoinAccounting` backed by in-memory maps.
    #[derive(Debug)]
    struct FakeAccounting {
        initialized: bool,
        balances: BTreeMap<Address, U256>,
        allowances: BTreeMap<(Address, Address), U256>,
        total_supply: U256,
        supply_cap: U256,
        name: String,
        symbol: String,
        currency: String,
        paused: U256,
        nonces: BTreeMap<Address, U256>,
        contract_uri: String,
        roles: BTreeMap<(B256, Address), bool>,
        role_member_counts: BTreeMap<B256, U256>,
        role_admins: BTreeMap<B256, B256>,
        policy_ids: BTreeMap<B256, u64>,
        events: Vec<LogData>,
    }

    impl FakeAccounting {
        fn new() -> Self {
            Self {
                initialized: true,
                balances: BTreeMap::new(),
                allowances: BTreeMap::new(),
                total_supply: U256::ZERO,
                supply_cap: B20_MAX_SUPPLY_CAP,
                name: "USD Coin".to_string(),
                symbol: "USDC".to_string(),
                currency: "USD".to_string(),
                paused: U256::ZERO,
                nonces: BTreeMap::new(),
                contract_uri: String::new(),
                roles: BTreeMap::new(),
                role_member_counts: BTreeMap::new(),
                role_admins: BTreeMap::new(),
                policy_ids: BTreeMap::new(),
                events: Vec::new(),
            }
        }
    }

    impl TokenAccounting for FakeAccounting {
        fn token_address(&self) -> Address {
            TOKEN
        }
        fn is_initialized(&self) -> Result<bool> {
            Ok(self.initialized)
        }
        fn balance_of(&self, account: Address) -> Result<U256> {
            Ok(self.balances.get(&account).copied().unwrap_or(U256::ZERO))
        }
        fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
            self.balances.insert(account, balance);
            Ok(())
        }
        fn allowance(&self, owner: Address, spender: Address) -> Result<U256> {
            Ok(self.allowances.get(&(owner, spender)).copied().unwrap_or(U256::ZERO))
        }
        fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
            self.allowances.insert((owner, spender), amount);
            Ok(())
        }
        fn total_supply(&self) -> Result<U256> {
            Ok(self.total_supply)
        }
        fn set_total_supply(&mut self, supply: U256) -> Result<()> {
            self.total_supply = supply;
            Ok(())
        }
        fn supply_cap(&self) -> Result<U256> {
            Ok(self.supply_cap)
        }
        fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
            self.supply_cap = cap;
            Ok(())
        }
        fn name(&self) -> Result<String> {
            Ok(self.name.clone())
        }
        fn set_name(&mut self, name: String) -> Result<()> {
            self.name = name;
            Ok(())
        }
        fn symbol(&self) -> Result<String> {
            Ok(self.symbol.clone())
        }
        fn set_symbol(&mut self, symbol: String) -> Result<()> {
            self.symbol = symbol;
            Ok(())
        }
        fn decimals(&self) -> Result<u8> {
            Ok(6)
        }
        fn paused(&self) -> Result<U256> {
            Ok(self.paused)
        }
        fn set_paused(&mut self, vectors: U256) -> Result<()> {
            self.paused = vectors;
            Ok(())
        }
        fn nonce(&self, owner: Address) -> Result<U256> {
            Ok(self.nonces.get(&owner).copied().unwrap_or(U256::ZERO))
        }
        fn increment_nonce(&mut self, owner: Address) -> Result<()> {
            let n = self.nonces.entry(owner).or_insert(U256::ZERO);
            *n += U256::ONE;
            Ok(())
        }
        fn contract_uri(&self) -> Result<String> {
            Ok(self.contract_uri.clone())
        }
        fn set_contract_uri(&mut self, uri: String) -> Result<()> {
            self.contract_uri = uri;
            Ok(())
        }
        fn has_role(&self, role: B256, account: Address) -> Result<bool> {
            Ok(self.roles.get(&(role, account)).copied().unwrap_or(false))
        }
        fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()> {
            self.roles.insert((role, account), enabled);
            Ok(())
        }
        fn role_member_count(&self, role: B256) -> Result<U256> {
            Ok(self.role_member_counts.get(&role).copied().unwrap_or(U256::ZERO))
        }
        fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()> {
            self.role_member_counts.insert(role, count);
            Ok(())
        }
        fn role_admin(&self, role: B256) -> Result<B256> {
            Ok(self.role_admins.get(&role).copied().unwrap_or(B256::ZERO))
        }
        fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()> {
            self.role_admins.insert(role, admin_role);
            Ok(())
        }
        fn policy_id(&self, policy_scope: B256) -> Result<u64> {
            Ok(self.policy_ids.get(&policy_scope).copied().unwrap_or(0))
        }
        fn set_policy_id(&mut self, policy_scope: B256, policy_id: u64) -> Result<()> {
            self.policy_ids.insert(policy_scope, policy_id);
            Ok(())
        }
        fn emit_event(&mut self, log: LogData) -> Result<()> {
            self.events.push(log);
            Ok(())
        }
    }

    impl StablecoinAccounting for FakeAccounting {
        fn currency(&self) -> Result<String> {
            Ok(self.currency.clone())
        }
        fn set_currency(&mut self, currency: String) -> Result<()> {
            self.currency = currency;
            Ok(())
        }
    }

    /// Minimal [`PolicyAccounting`] backed by in-memory maps.
    #[derive(Debug)]
    struct FakePolicyAccounting {
        caller: Address,
        initialized: bool,
        policies: BTreeMap<u64, U256>,
        members: BTreeMap<(u64, Address), bool>,
        pending_admins: BTreeMap<u64, Address>,
        next_counter: u64,
        events: Vec<LogData>,
    }

    impl FakePolicyAccounting {
        fn new() -> Self {
            Self {
                caller: ADMIN,
                initialized: false,
                policies: BTreeMap::new(),
                members: BTreeMap::new(),
                pending_admins: BTreeMap::new(),
                next_counter: 0,
                events: Vec::new(),
            }
        }

        fn allow(&mut self, policy_id: u64, account: Address) {
            self.create_existing_policy(policy_id);
            self.members.insert((policy_id, account), true);
        }

        fn create_existing_policy(&mut self, policy_id: u64) {
            self.policies.insert(policy_id, PackedPolicy::new(Address::ZERO).into_u256());
        }
    }

    impl PolicyAccounting for FakePolicyAccounting {
        fn registry_address(&self) -> Address {
            Address::repeat_byte(0x02)
        }
        fn caller(&self) -> Address {
            self.caller
        }
        fn read_policy_word(&self, policy_id: u64) -> Result<U256> {
            Ok(self.policies.get(&policy_id).copied().unwrap_or(U256::ZERO))
        }
        fn write_policy_word(&mut self, policy_id: u64, word: U256) -> Result<()> {
            self.policies.insert(policy_id, word);
            Ok(())
        }
        fn read_member(&self, policy_id: u64, account: Address) -> Result<bool> {
            Ok(self.members.get(&(policy_id, account)).copied().unwrap_or(false))
        }
        fn set_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
            self.members.insert((policy_id, account), true);
            Ok(())
        }
        fn delete_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
            self.members.remove(&(policy_id, account));
            Ok(())
        }
        fn read_pending_admin(&self, policy_id: u64) -> Result<Address> {
            Ok(self.pending_admins.get(&policy_id).copied().unwrap_or(Address::ZERO))
        }
        fn write_pending_admin(&mut self, policy_id: u64, admin: Address) -> Result<()> {
            self.pending_admins.insert(policy_id, admin);
            Ok(())
        }
        fn delete_pending_admin(&mut self, policy_id: u64) -> Result<()> {
            self.pending_admins.remove(&policy_id);
            Ok(())
        }
        fn read_next_counter(&self) -> Result<u64> {
            Ok(self.next_counter)
        }
        fn write_next_counter(&mut self, counter: u64) -> Result<()> {
            self.next_counter = counter;
            Ok(())
        }
        fn emit_event(&mut self, log: LogData) -> Result<()> {
            self.events.push(log);
            Ok(())
        }
        fn mark_initialized(&mut self) -> Result<()> {
            self.initialized = true;
            Ok(())
        }
    }

    type Tok = B20StablecoinToken<FakeAccounting, FakePolicyAccounting>;

    fn token() -> Tok {
        B20StablecoinToken::with_storage_and_policy(
            FakeAccounting::new(),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
    }

    /// Grants `role` to `account` and keeps the admin member-count consistent.
    fn grant(tok: &mut Tok, role: B256, account: Address) {
        tok.accounting_mut().set_role(role, account, true).unwrap();
        let next = tok.accounting().role_member_count(role).unwrap() + U256::ONE;
        tok.accounting_mut().set_role_member_count(role, next).unwrap();
    }

    /// Credits `account` with `amount` and grows total supply to match.
    fn fund(tok: &mut Tok, account: Address, amount: U256) {
        let bal = tok.accounting().balance_of(account).unwrap();
        tok.accounting_mut().set_balance(account, bal + amount).unwrap();
        let supply = tok.accounting().total_supply().unwrap();
        tok.accounting_mut().set_total_supply(supply + amount).unwrap();
    }

    fn last_event_sig(tok: &Tok) -> B256 {
        tok.accounting().events.last().unwrap().topics()[0]
    }

    fn anvil_owner() -> Address {
        let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    /// Produces a validly-signed `PermitArgs` for the token's current domain and `owner` nonce.
    fn signed_permit(
        tok: &Tok,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
    ) -> PermitArgs {
        let domain_sep = LOGIC.domain_separator(tok, CHAIN_ID).unwrap();
        let nonce = tok.accounting().nonce(owner).unwrap();
        let mut args =
            PermitArgs { owner, spender, value, deadline, v: 0, r: B256::ZERO, s: B256::ZERO };
        let signing_hash = args.signing_hash(domain_sep, nonce);
        let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
        let (sig, recid) = key.sign_prehash_recoverable(signing_hash.as_slice()).unwrap();
        let bytes = sig.to_bytes();
        args.r = B256::from_slice(&bytes[..32]);
        args.s = B256::from_slice(&bytes[32..]);
        args.v = if recid.is_y_odd() { 28 } else { 27 };
        args
    }

    // --- transfer ---

    #[test]
    fn transfer_moves_balance_and_emits_transfer() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(30u64), true).unwrap();
        assert_eq!(tok.accounting().balance_of(ALICE).unwrap(), U256::from(70u64));
        assert_eq!(tok.accounting().balance_of(BOB).unwrap(), U256::from(30u64));
        assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
    }

    #[test]
    fn transfer_reverts_on_zero_receiver() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(10u64));
        let err =
            LOGIC.transfer(&mut tok, ALICE, Address::ZERO, U256::from(1u64), true).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO })
        );
    }

    #[test]
    fn transfer_reverts_on_insufficient_balance() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(10u64));
        let err = LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(50u64), true).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: ALICE,
                balance: U256::from(10u64),
                needed: U256::from(50u64),
            })
        );
    }

    #[test]
    fn transfer_reverts_when_paused() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(10u64));
        LOGIC.pause(&mut tok, ADMIN, vec![IB20::PausableFeature::TRANSFER], true).unwrap();
        let err = LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(1u64), true).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::TRANSFER,
            })
        );
    }

    #[test]
    fn transfer_unprivileged_enforces_transfer_policies() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        // ALLOWLIST with no members → sender/receiver policy checks revert.
        const POLICY: u64 = (1u64 << 56) | 7;
        tok.accounting_mut().set_policy_id(B20PolicyType::TransferSender.id(), POLICY).unwrap();
        tok.accounting_mut().set_policy_id(B20PolicyType::TransferReceiver.id(), POLICY).unwrap();
        tok.policy_storage_mut().create_existing_policy(POLICY);
        assert!(LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).is_err());
        // Authorize both parties → transfer succeeds through the guard path.
        tok.policy_storage_mut().allow(POLICY, ALICE);
        tok.policy_storage_mut().allow(POLICY, BOB);
        LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).unwrap();
        assert_eq!(tok.accounting().balance_of(BOB).unwrap(), U256::from(10u64));
    }

    // --- transfer_from ---

    #[test]
    fn transfer_from_decrements_finite_allowance() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        tok.accounting_mut().set_allowance(ALICE, BOB, U256::from(40u64)).unwrap();
        LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
        assert_eq!(tok.accounting().allowance(ALICE, BOB).unwrap(), U256::from(10u64));
        assert_eq!(tok.accounting().balance_of(BOB).unwrap(), U256::from(30u64));
    }

    #[test]
    fn transfer_from_infinite_allowance_is_not_decremented() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        tok.accounting_mut().set_allowance(ALICE, BOB, U256::MAX).unwrap();
        LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
        assert_eq!(tok.accounting().allowance(ALICE, BOB).unwrap(), U256::MAX);
    }

    #[test]
    fn transfer_from_reverts_on_insufficient_allowance() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        tok.accounting_mut().set_allowance(ALICE, BOB, U256::from(5u64)).unwrap();
        let err =
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender: BOB,
                allowance: U256::from(5u64),
                needed: U256::from(30u64),
            })
        );
    }

    // --- approve ---

    #[test]
    fn approve_sets_allowance_and_emits() {
        let mut tok = token();
        LOGIC.approve(&mut tok, ALICE, BOB, U256::from(50u64)).unwrap();
        assert_eq!(tok.accounting().allowance(ALICE, BOB).unwrap(), U256::from(50u64));
        assert_eq!(last_event_sig(&tok), IB20::Approval::SIGNATURE_HASH);
    }

    #[test]
    fn approve_reverts_on_zero_spender() {
        let mut tok = token();
        let err = LOGIC.approve(&mut tok, ALICE, Address::ZERO, U256::from(1u64)).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::InvalidSpender { spender: Address::ZERO })
        );
    }

    // --- mint ---

    #[test]
    fn mint_privileged_increases_supply_and_balance() {
        let mut tok = token();
        tok.policy_storage_mut().allow(0, BOB); // MintReceiver policy is enforced even when privileged
        LOGIC.mint(&mut tok, ADMIN, BOB, U256::from(100u64), true).unwrap();
        assert_eq!(tok.accounting().balance_of(BOB).unwrap(), U256::from(100u64));
        assert_eq!(tok.accounting().total_supply().unwrap(), U256::from(100u64));
        assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
    }

    #[test]
    fn mint_reverts_over_supply_cap() {
        let mut tok = token();
        tok.accounting_mut().set_supply_cap(U256::from(50u64)).unwrap();
        let err = LOGIC.mint(&mut tok, ADMIN, BOB, U256::from(100u64), true).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::SupplyCapExceeded {
                cap: U256::from(50u64),
                attempted: U256::from(100u64),
            })
        );
    }

    #[test]
    fn mint_unprivileged_requires_mint_role() {
        let mut tok = token();
        let err = LOGIC.mint(&mut tok, ALICE, BOB, U256::from(1u64), false).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: B20TokenRole::Mint.id(),
            })
        );
    }

    // --- burn / burn_blocked ---

    #[test]
    fn burn_requires_role_then_decreases_supply() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        let err = LOGIC.burn(&mut tok, ALICE, U256::from(1u64)).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: B20TokenRole::Burn.id(),
            })
        );
        grant(&mut tok, B20TokenRole::Burn.id(), ALICE);
        LOGIC.burn(&mut tok, ALICE, U256::from(40u64)).unwrap();
        assert_eq!(tok.accounting().balance_of(ALICE).unwrap(), U256::from(60u64));
        assert_eq!(tok.accounting().total_supply().unwrap(), U256::from(60u64));
        assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
    }

    #[test]
    fn burn_blocked_destroys_from_unauthorized_account() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        // ALWAYS_BLOCK => ALICE is unauthorized/blocked; privileged skips the role check.
        tok.accounting_mut()
            .set_policy_id(
                B20PolicyType::TransferSender.id(),
                PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            )
            .unwrap();
        LOGIC.burn_blocked(&mut tok, ADMIN, ALICE, U256::from(40u64), true).unwrap();
        assert_eq!(tok.accounting().balance_of(ALICE).unwrap(), U256::from(60u64));
        assert_eq!(last_event_sig(&tok), IB20::BurnedBlocked::SIGNATURE_HASH);
    }

    #[test]
    fn burn_blocked_reverts_when_account_not_blocked() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(100u64));
        // Default ALWAYS_ALLOW authorizes ALICE => not blocked.
        let err = LOGIC.burn_blocked(&mut tok, ADMIN, ALICE, U256::from(1u64), true).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotBlocked { account: ALICE }));
    }

    // --- pause ---

    #[test]
    fn pause_and_unpause_toggle_feature_bit() {
        let mut tok = token();
        LOGIC.pause(&mut tok, ADMIN, vec![IB20::PausableFeature::MINT], true).unwrap();
        assert!(LOGIC.is_paused(&tok, IB20::PausableFeature::MINT).unwrap());
        assert!(!LOGIC.is_paused(&tok, IB20::PausableFeature::TRANSFER).unwrap());
        LOGIC.unpause(&mut tok, ADMIN, vec![IB20::PausableFeature::MINT], true).unwrap();
        assert!(!LOGIC.is_paused(&tok, IB20::PausableFeature::MINT).unwrap());
    }

    #[test]
    fn pause_reverts_on_empty_feature_set() {
        let mut tok = token();
        let err = LOGIC.pause(&mut tok, ADMIN, vec![], true).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
    }

    #[test]
    fn paused_features_reports_active_set() {
        let mut tok = token();
        LOGIC
            .pause(
                &mut tok,
                ADMIN,
                vec![IB20::PausableFeature::MINT, IB20::PausableFeature::BURN],
                true,
            )
            .unwrap();
        assert_eq!(LOGIC.paused_features(&tok).unwrap().len(), 2);
        assert!(LOGIC.is_paused(&tok, IB20::PausableFeature::BURN).unwrap());
    }

    // --- config / metadata ---

    #[test]
    fn update_supply_cap_sets_and_emits() {
        let mut tok = token();
        LOGIC.update_supply_cap(&mut tok, ADMIN, U256::from(1_000u64), true).unwrap();
        assert_eq!(tok.accounting().supply_cap().unwrap(), U256::from(1_000u64));
        assert_eq!(last_event_sig(&tok), IB20::SupplyCapUpdated::SIGNATURE_HASH);
    }

    #[test]
    fn update_supply_cap_reverts_below_current_supply() {
        let mut tok = token();
        fund(&mut tok, ALICE, U256::from(500u64));
        let err = LOGIC.update_supply_cap(&mut tok, ADMIN, U256::from(100u64), true).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::InvalidSupplyCap {
                currentSupply: U256::from(500u64),
                proposedCap: U256::from(100u64),
            })
        );
    }

    #[test]
    fn update_name_emits_name_updated_and_domain_changed() {
        let mut tok = token();
        LOGIC.update_name(&mut tok, ADMIN, "New Name".to_string(), true).unwrap();
        assert_eq!(tok.accounting().name().unwrap(), "New Name");
        let events = &tok.accounting().events;
        assert_eq!(events[events.len() - 2].topics()[0], IB20::NameUpdated::SIGNATURE_HASH);
        assert_eq!(events[events.len() - 1].topics()[0], IB20::EIP712DomainChanged::SIGNATURE_HASH);
    }

    // --- roles ---

    #[test]
    fn grant_role_privileged_grants_and_emits() {
        let mut tok = token();
        LOGIC.grant_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
        assert!(tok.accounting().has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
        assert_eq!(last_event_sig(&tok), IB20::RoleGranted::SIGNATURE_HASH);
    }

    #[test]
    fn revoke_role_privileged_revokes_and_emits() {
        let mut tok = token();
        grant(&mut tok, B20TokenRole::Mint.id(), ALICE);
        LOGIC.revoke_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
        assert!(!tok.accounting().has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
        assert_eq!(last_event_sig(&tok), IB20::RoleRevoked::SIGNATURE_HASH);
    }

    #[test]
    fn revoke_last_admin_is_rejected() {
        let mut tok = token();
        grant(&mut tok, B20TokenRole::DefaultAdmin.id(), ADMIN);
        let err = LOGIC
            .revoke_role(&mut tok, ADMIN, B20TokenRole::DefaultAdmin.id(), ADMIN, true)
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
    }

    #[test]
    fn grant_role_unchecked_bumps_admin_count() {
        let mut tok = token();
        LOGIC
            .grant_role_unchecked(&mut tok, B20TokenRole::DefaultAdmin.id(), ADMIN, TOKEN)
            .unwrap();
        assert!(tok.accounting().has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
        assert_eq!(
            tok.accounting().role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(),
            U256::ONE
        );
    }

    // --- policy ---

    #[test]
    fn update_policy_sets_new_id() {
        let mut tok = token();
        tok.policy_storage_mut().create_existing_policy(7);
        LOGIC.update_policy(&mut tok, ADMIN, B20PolicyType::TransferSender.id(), 7, true).unwrap();
        assert_eq!(tok.accounting().policy_id(B20PolicyType::TransferSender.id()).unwrap(), 7);
    }

    #[test]
    fn update_policy_reverts_when_policy_missing() {
        let mut tok = token();
        let err = LOGIC
            .update_policy(&mut tok, ADMIN, B20PolicyType::TransferSender.id(), 99, true)
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IB20::PolicyNotFound { policyId: 99 }));
    }

    #[test]
    fn policy_id_rejects_unsupported_scope() {
        let tok = token();
        let scope = B256::repeat_byte(0xEE);
        let err = LOGIC.policy_id(&tok, scope).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: scope })
        );
    }

    // --- permit ---

    #[test]
    fn permit_sets_allowance_and_increments_nonce() {
        let mut tok = token();
        let owner = anvil_owner();
        let args = signed_permit(&tok, owner, BOB, U256::from(500u64), U256::MAX);
        LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args).unwrap();
        assert_eq!(tok.accounting().allowance(owner, BOB).unwrap(), U256::from(500u64));
        assert_eq!(tok.accounting().nonce(owner).unwrap(), U256::ONE);
    }

    #[test]
    fn permit_reverts_when_expired() {
        let mut tok = token();
        let owner = anvil_owner();
        let args = signed_permit(&tok, owner, BOB, U256::from(1u64), U256::from(10u64));
        let err = LOGIC.permit(&mut tok, CHAIN_ID, U256::from(11u64), args).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::ExpiredSignature { deadline: U256::from(10u64) })
        );
    }

    #[test]
    fn permit_replay_is_rejected() {
        let mut tok = token();
        let owner = anvil_owner();
        let args = signed_permit(&tok, owner, BOB, U256::from(1u64), U256::MAX);
        LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args.clone()).unwrap();
        // Same (v, r, s): the nonce has advanced, so recovery no longer matches `owner`.
        assert!(LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args).is_err());
    }

    // --- reads ---

    #[test]
    fn currency_reads_storage() {
        let tok = token();
        assert_eq!(LOGIC.currency(&tok).unwrap(), "USD");
    }

    #[test]
    fn is_initialized_reflects_storage() {
        let mut tok = token();
        assert!(LOGIC.is_initialized(&tok).unwrap());
        tok.accounting_mut().initialized = false;
        assert!(!LOGIC.is_initialized(&tok).unwrap());
    }

    #[test]
    fn domain_separator_is_deterministic_and_chain_specific() {
        let tok = token();
        assert_eq!(
            LOGIC.domain_separator(&tok, CHAIN_ID).unwrap(),
            LOGIC.domain_separator(&tok, CHAIN_ID).unwrap()
        );
        assert_ne!(
            LOGIC.domain_separator(&tok, 1).unwrap(),
            LOGIC.domain_separator(&tok, 2).unwrap()
        );
    }
}
