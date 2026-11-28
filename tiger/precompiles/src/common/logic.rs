//! Business logic shared by the asset and stablecoin B-20 precompiles, for every version.
//!
//! Both token kinds run the same B-20 core; the asset kind adds its own methods (in
//! `b20_asset::logic`). One implementation serves every version: V1 (Beryl), V2 (Cobalt: seizing,
//! metered permit hashing, one read for all transfer policy ids) and V3 (Denim: the transfer
//! executor policy on every transfer path, and no credit to the token itself). Each difference is
//! a check on [`B20Logic::version`]; selectors a version does not declare never reach here, since
//! the dispatcher gates them on the version's ABI.

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use alloy_primitives::{Address, B256, FixedBytes, U256, b256, keccak256};
use alloy_sol_types::{SolEvent, SolValue};

use crate::{
    B20_MAX_SUPPLY_CAP, B20CreditRecipientStrategy, B20PausableFeature, B20PolicyType, B20TokenRole,
    B20Version, BasePrecompileError, Eip712Domain, IB20, NonZeroAddress, PermitArgs,
    PolicyAccounting, PolicyRegistry, PolicyVersion, Result, TokenAccounting, TransferPolicyIds,
};

/// A B-20 token: its storage, the policy registry's storage, and the registry's logic.
#[derive(Debug, Clone)]
pub struct B20Token<S, A> {
    /// The token's storage.
    pub accounting: S,
    /// The policy registry's storage.
    pub policy: A,
    /// The policy registry's logic, of the version the fork activates.
    pub registry: PolicyRegistry,
}

impl<S: TokenAccounting, A: PolicyAccounting> B20Token<S, A> {
    /// Creates a token backed by token storage, policy-registry storage, and version.
    pub const fn with_storage_and_policy(
        accounting: S,
        policy: A,
        policy_version: PolicyVersion,
    ) -> Self {
        Self { accounting, policy, registry: PolicyRegistry::new(policy_version) }
    }
}

/// The B-20 logic of one version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B20Logic {
    /// The active version.
    pub version: B20Version,
    /// Whether the token is a stablecoin (its V1 `updatePolicy` reads the old policy id before
    /// checking the new one exists; the asset's reads it after).
    pub stablecoin: bool,
}

impl B20Logic {
    /// `keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")`
    pub const DOMAIN_TYPEHASH: B256 =
        b256!("8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f");

    /// EIP-712 domain version string pinned to `"1"`.
    pub const EIP712_VERSION: &[u8] = b"1";

    /// The asset logic of `version`.
    pub const fn asset(version: B20Version) -> Self {
        Self { version, stablecoin: false }
    }

    /// The stablecoin logic of `version`.
    pub const fn stablecoin(version: B20Version) -> Self {
        Self { version, stablecoin: true }
    }

    /// The features `pause`/`unpause`/`isPaused` accept: `SEIZE` from V2.
    fn pausable_features(self) -> &'static [IB20::PausableFeature] {
        const V1: &[IB20::PausableFeature] = &[
            IB20::PausableFeature::TRANSFER,
            IB20::PausableFeature::MINT,
            IB20::PausableFeature::BURN,
        ];
        const V2: &[IB20::PausableFeature] = &[
            IB20::PausableFeature::TRANSFER,
            IB20::PausableFeature::MINT,
            IB20::PausableFeature::BURN,
            IB20::PausableFeature::SEIZE,
        ];
        if self.version >= B20Version::V2 { V2 } else { V1 }
    }

    /// Hashes with gas metering from V2; V1's (Beryl's) schedule hashes unmetered.
    fn keccak256<S: TokenAccounting>(self, accounting: &S, data: &[u8]) -> Result<B256> {
        if self.version >= B20Version::V2 {
            accounting.metered_keccak256(data)
        } else {
            Ok(keccak256(data))
        }
    }

    /// Validates `to` as a credit recipient: not zero, and from V3 not the token itself.
    fn ensure_recipient<S: TokenAccounting, A: PolicyAccounting>(
        self,
        token: &B20Token<S, A>,
        to: Address,
    ) -> Result<()> {
        let invalid = if self.version >= B20Version::V3 {
            B20CreditRecipientStrategy::ExcludingZeroAndSelf
                .recipient(to, token.accounting.token_address())
                .is_err()
        } else {
            to == Address::ZERO
        };
        if invalid {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        Ok(())
    }

    /// Checks the transfer policies, then moves the balance. `policies` holds all three transfer
    /// policy ids, read in one SLOAD (V2+); `None` skips the checks (factory-privileged path).
    /// V2 checks the sender and receiver; V3 checks the executor first, and the sender unless it
    /// is the executor under the same policy.
    fn transfer_inner<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        policies: Option<&TransferPolicyIds>,
    ) -> Result<()> {
        if let Some(policies) = policies {
            let v3 = self.version >= B20Version::V3;
            if v3 {
                token.ensure_authorized_by_id(
                    B20PolicyType::TransferExecutor.id(),
                    policies.executor,
                    caller,
                )?;
            }
            if !v3 || caller != from || policies.executor != policies.sender {
                token.ensure_authorized_by_id(
                    B20PolicyType::TransferSender.id(),
                    policies.sender,
                    from,
                )?;
            }
            token.ensure_authorized_by_id(
                B20PolicyType::TransferReceiver.id(),
                policies.receiver,
                to,
            )?;
        }
        self.move_balance(token, from, to, amount)
    }

    /// V1's transfer core: zero checks, then one read per transfer policy (unless privileged).
    fn transfer_inner_v1<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
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
            token.ensure_policy_type(B20PolicyType::TransferSender, from)?;
            token.ensure_policy_type(B20PolicyType::TransferReceiver, to)?;
        }
        self.move_balance(token, from, to, amount)
    }

    /// Debits `from`, credits `to`, and emits `Transfer(from, to, amount)`, with no policy,
    /// allowance, pause, or zero-address checks: callers apply their own guards first.
    fn move_balance<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<()> {
        let from_balance = token.accounting.balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance: from_balance,
                needed: amount,
            }));
        }
        let new_from_balance =
            from_balance.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting.set_balance(from, new_from_balance)?;
        let to_balance = token.accounting.balance_of(to)?;
        let new_to_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting.set_balance(to, new_to_balance)?;
        token.accounting.emit_event(IB20::Transfer { from, to, amount }.encode_log_data())
    }

    /// Supply-reducing core of the burn operations, without pause or role checks.
    fn burn_inner<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        from: Address,
        amount: U256,
    ) -> Result<()> {
        let balance = token.accounting.balance_of(from)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance,
                needed: amount,
            }));
        }
        token.accounting.set_balance(from, balance - amount)?;
        let supply = token.accounting.total_supply()?;
        let new_supply =
            supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting.set_total_supply(new_supply)?;
        token
            .accounting
            .emit_event(IB20::Transfer { from, to: Address::ZERO, amount }.encode_log_data())
    }

    /// Revokes `role` from `account` without checking caller authorization.
    fn revoke_role_unchecked<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if !token.accounting.has_role(role, account)? {
            return Ok(());
        }
        token.accounting.set_role(role, account, false)?;
        if role == B20TokenRole::DefaultAdmin.id() {
            let current = token.accounting.role_member_count(role)?;
            let next =
                current.checked_sub(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
            token.accounting.set_role_member_count(role, next)?;
        }
        token.accounting.emit_event(IB20::RoleRevoked { role, account, sender }.encode_log_data())
    }

    /// Ensures role-admin mutations are still reachable.
    fn ensure_role_admin_mutations_available<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &B20Token<S, A>,
        caller: Address,
    ) -> Result<()> {
        let admin_role = B20TokenRole::DefaultAdmin.id();
        if token.accounting.role_member_count(admin_role)? == U256::ZERO {
            return Err(BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: caller,
                neededRole: admin_role,
            }));
        }
        Ok(())
    }

    /// Ensures `policy_scope` names a built-in B-20 policy slot this version supports: the seize
    /// scopes came at Cobalt (V2), so V1 rejects them, matching the base-std `v1.0.0` reference.
    ///
    /// The match is exhaustive on purpose: a scope added to `B20PolicyType` for a future fork
    /// fails to compile here until its versions are decided.
    fn ensure_supported_policy_type(self, policy_scope: B256) -> Result<()> {
        let supported = match B20PolicyType::from_id(policy_scope) {
            Some(
                B20PolicyType::TransferSender
                | B20PolicyType::TransferReceiver
                | B20PolicyType::TransferExecutor
                | B20PolicyType::MintReceiver,
            ) => true,
            Some(B20PolicyType::SeizeExempt | B20PolicyType::SeizeReceiver) => {
                self.version >= B20Version::V2
            }
            None => false,
        };
        if !supported {
            return Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }));
        }
        Ok(())
    }

    /// Moves `amount` from the caller to `to`.
    pub fn transfer<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        token.ensure_not_paused(IB20::PausableFeature::TRANSFER)?;
        if self.version == B20Version::V1 {
            return self.transfer_inner_v1(token, caller, to, amount, privileged);
        }
        self.ensure_recipient(token, to)?;
        NonZeroAddress::new(caller)
            .map_err(|_| BasePrecompileError::revert(IB20::InvalidSender { sender: caller }))?;
        if privileged {
            return self.transfer_inner(token, caller, caller, to, amount, None);
        }
        let policies = token.accounting.transfer_policy_ids()?;
        self.transfer_inner(token, caller, caller, to, amount, Some(&policies))
    }

    /// Moves `amount` from `from` to `to` on the caller's allowance.
    pub fn transfer_from<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        token.ensure_not_paused(IB20::PausableFeature::TRANSFER)?;
        // Validate before allowance / transfer-policy-id SLOADs.
        self.ensure_recipient(token, to)?;
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        let allowance = token.accounting.allowance(from, caller)?;
        let is_infinite = allowance == U256::MAX;
        if !is_infinite && allowance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender: caller,
                allowance,
                needed: amount,
            }));
        }
        if self.version == B20Version::V1 {
            if !privileged && caller != from {
                token.ensure_policy_type(B20PolicyType::TransferExecutor, caller)?;
            }
            self.transfer_inner_v1(token, from, to, amount, privileged)?;
        } else if privileged {
            self.transfer_inner(token, caller, from, to, amount, None)?;
        } else {
            // One SLOAD fetches all transfer policy ids for the executor and sender/receiver checks.
            let policies = token.accounting.transfer_policy_ids()?;
            if self.version == B20Version::V2 && caller != from {
                token.ensure_authorized_by_id(
                    B20PolicyType::TransferExecutor.id(),
                    policies.executor,
                    caller,
                )?;
            }
            self.transfer_inner(token, caller, from, to, amount, Some(&policies))?;
        }
        if is_infinite {
            return Ok(());
        }
        token.accounting.set_allowance(from, caller, allowance - amount)
    }

    /// Sets the caller's allowance for `spender`.
    pub fn approve<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
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
        token.accounting.set_allowance(caller, spender, amount)?;
        token
            .accounting
            .emit_event(IB20::Approval { owner: caller, spender, amount }.encode_log_data())
    }

    /// Emits `Memo(caller, memo)`.
    pub fn emit_memo<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        memo: B256,
    ) -> Result<()> {
        token.accounting.emit_event(IB20::Memo { caller, memo }.encode_log_data())
    }

    /// Mints `amount` to `to`.
    pub fn mint<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        token.ensure_not_paused(IB20::PausableFeature::MINT)?;
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::Mint)?;
        }
        self.ensure_recipient(token, to)?;
        token.ensure_policy_type(B20PolicyType::MintReceiver, to)?;
        let supply = token.accounting.total_supply()?;
        let cap = token.accounting.supply_cap()?;
        let new_supply =
            supply.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        if new_supply > cap {
            return Err(BasePrecompileError::revert(IB20::SupplyCapExceeded {
                cap,
                attempted: new_supply,
            }));
        }
        token.accounting.set_total_supply(new_supply)?;
        let to_balance = token.accounting.balance_of(to)?;
        let new_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        token.accounting.set_balance(to, new_balance)?;
        token
            .accounting
            .emit_event(IB20::Transfer { from: Address::ZERO, to, amount }.encode_log_data())
    }

    /// Burns `amount` of the caller's own balance (never factory-privileged).
    pub fn burn<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        amount: U256,
    ) -> Result<()> {
        token.ensure_not_paused(IB20::PausableFeature::BURN)?;
        token.ensure_token_role(caller, B20TokenRole::Burn)?;
        self.burn_inner(token, caller, amount)
    }

    /// Burns `amount` from `from`, which must be blocked by the sender policy.
    pub fn burn_blocked<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        token.ensure_not_paused(IB20::PausableFeature::BURN)?;
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::BurnBlocked)?;
        }
        token.ensure_blocked(from)?;
        self.burn_inner(token, from, amount)?;
        token.accounting.emit_event(IB20::BurnedBlocked { caller, from, amount }.encode_log_data())
    }

    /// (V2) Moves `amount` from a seizable `from` to `to`, with a memo.
    pub fn seize_with_memo<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        token.ensure_not_paused(IB20::PausableFeature::SEIZE)?;
        token.ensure_token_role(caller, B20TokenRole::Seize)?;
        // A valid recipient guards against a disguised burn (and from V3 a balance stranded on
        // this token); `from != 0` guards against a disguised mint, matching `transfer_inner`.
        self.ensure_recipient(token, to)?;
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        if from == to {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        token.ensure_seizable(from)?;
        // Gate the destination like `mint` gates `MintReceiver`: an unset scope is always-allow, so
        // a treasury need not be allowlisted by default.
        token.ensure_policy_type(B20PolicyType::SeizeReceiver, to)?;
        self.move_balance(token, from, to, amount)?;
        // `Memo` must immediately follow the `Transfer` (from `move_balance`), before `Seized`.
        self.emit_memo(token, caller, memo)?;
        token.accounting.emit_event(IB20::Seized { caller, from, to, amount }.encode_log_data())
    }

    /// Sets (`pause`) or clears the pause bits of `features`.
    fn set_paused<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
        pause: bool,
    ) -> Result<()> {
        for feature in &features {
            B20PausableFeature::ensure_one_of(*feature, self.pausable_features())?;
        }
        if !privileged {
            let role = if pause { B20TokenRole::Pause } else { B20TokenRole::Unpause };
            token.ensure_token_role(caller, role)?;
        }
        if features.is_empty() {
            return Err(BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
        }
        let mut next = token.accounting.paused()?;
        for feature in &features {
            if pause {
                next |= B20PausableFeature::mask(*feature);
            } else {
                next &= !B20PausableFeature::mask(*feature);
            }
        }
        token.accounting.set_paused(next)?;
        let log = if pause {
            IB20::Paused { updater: caller, features }.encode_log_data()
        } else {
            IB20::Unpaused { updater: caller, features }.encode_log_data()
        };
        token.accounting.emit_event(log)
    }

    /// Pauses `features`.
    pub fn pause<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        self.set_paused(token, caller, features, privileged, true)
    }

    /// Unpauses `features`.
    pub fn unpause<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        self.set_paused(token, caller, features, privileged, false)
    }

    /// Sets the supply cap, which must lie between the supply and [`B20_MAX_SUPPLY_CAP`].
    pub fn update_supply_cap<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::DefaultAdmin)?;
        }
        let supply = token.accounting.total_supply()?;
        if new_cap < supply || new_cap > B20_MAX_SUPPLY_CAP {
            return Err(BasePrecompileError::revert(IB20::InvalidSupplyCap {
                currentSupply: supply,
                proposedCap: new_cap,
            }));
        }
        let old = token.accounting.supply_cap()?;
        token.accounting.set_supply_cap(new_cap)?;
        token.accounting.emit_event(
            IB20::SupplyCapUpdated { updater: caller, oldSupplyCap: old, newSupplyCap: new_cap }
                .encode_log_data(),
        )
    }

    /// Sets the token name (which changes the EIP-712 domain).
    pub fn update_name<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::Metadata)?;
        }
        token.accounting.set_name(name.clone())?;
        token
            .accounting
            .emit_event(IB20::NameUpdated { updater: caller, newName: name }.encode_log_data())?;
        token.accounting.emit_event(IB20::EIP712DomainChanged {}.encode_log_data())
    }

    /// Sets the token symbol.
    pub fn update_symbol<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::Metadata)?;
        }
        token.accounting.set_symbol(symbol.clone())?;
        token.accounting.emit_event(
            IB20::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data(),
        )
    }

    /// Sets the contract URI.
    pub fn update_contract_uri<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::Metadata)?;
        }
        token.accounting.set_contract_uri(uri)?;
        token.accounting.emit_event(IB20::ContractURIUpdated {}.encode_log_data())
    }

    /// Grants `role` to `account`.
    pub fn grant_role<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        if role == B20TokenRole::DefaultAdmin.id() || !privileged {
            self.ensure_role_admin_mutations_available(token, caller)?;
        }
        if !privileged {
            let admin = token.accounting.role_admin(role)?;
            token.ensure_role(caller, admin)?;
        }
        self.grant_role_unchecked(token, role, account, caller)
    }

    /// Grants `role` to `account` without checking caller authorization.
    pub fn grant_role_unchecked<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if token.accounting.has_role(role, account)? {
            return Ok(());
        }
        token.accounting.set_role(role, account, true)?;
        if role == B20TokenRole::DefaultAdmin.id() {
            let current = token.accounting.role_member_count(role)?;
            let next =
                current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
            token.accounting.set_role_member_count(role, next)?;
        }
        token.accounting.emit_event(IB20::RoleGranted { role, account, sender }.encode_log_data())
    }

    /// Revokes `role` from `account`; the last default admin cannot be revoked.
    pub fn revoke_role<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            self.ensure_role_admin_mutations_available(token, caller)?;
            let admin = token.accounting.role_admin(role)?;
            token.ensure_role(caller, admin)?;
        }
        if role == B20TokenRole::DefaultAdmin.id()
            && token.accounting.has_role(role, account)?
            && token.accounting.role_member_count(role)? == U256::ONE
        {
            return Err(BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
        }
        self.revoke_role_unchecked(token, role, account, caller)
    }

    /// Renounces the caller's `role`; the last default admin must use `renounceLastAdmin`.
    pub fn renounce_role<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()> {
        if confirmation != caller {
            return Err(BasePrecompileError::revert(IB20::AccessControlBadConfirmation {}));
        }
        if role == B20TokenRole::DefaultAdmin.id()
            && token.accounting.has_role(role, caller)?
            && token.accounting.role_member_count(role)? == U256::ONE
        {
            return Err(BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
        }
        self.revoke_role_unchecked(token, role, caller, caller)
    }

    /// The sole default admin gives up the role, leaving the token without one.
    pub fn renounce_last_admin<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
    ) -> Result<()> {
        let admin_role = B20TokenRole::DefaultAdmin.id();
        token.ensure_role(caller, admin_role)?;
        if token.accounting.role_member_count(admin_role)? != U256::ONE {
            return Err(BasePrecompileError::revert(IB20::NotSoleAdmin {}));
        }
        self.revoke_role_unchecked(token, admin_role, caller, caller)?;
        token
            .accounting
            .emit_event(IB20::LastAdminRenounced { previousAdmin: caller }.encode_log_data())
    }

    /// Sets the admin role of `role`.
    pub fn set_role_admin<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()> {
        let previous_admin_role = token.accounting.role_admin(role)?;
        if !privileged {
            self.ensure_role_admin_mutations_available(token, caller)?;
            token.ensure_role(caller, previous_admin_role)?;
        }
        token.accounting.set_role_admin(role, new_admin_role)?;
        token.accounting.emit_event(
            IB20::RoleAdminChanged {
                role,
                previousAdminRole: previous_admin_role,
                newAdminRole: new_admin_role,
            }
            .encode_log_data(),
        )
    }

    /// Points the policy scope `policy_scope` at `new_policy_id`, which must exist.
    pub fn update_policy<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            token.ensure_token_role(caller, B20TokenRole::DefaultAdmin)?;
        }
        self.ensure_supported_policy_type(policy_scope)?;
        let early_read = self.stablecoin && self.version == B20Version::V1;
        let early_old_policy_id =
            if early_read { Some(token.accounting.policy_id(policy_scope)?) } else { None };
        if !token.registry.policy_exists(&token.policy, new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        let old_policy_id = match early_old_policy_id {
            Some(id) => id,
            None => token.accounting.policy_id(policy_scope)?,
        };
        token.accounting.set_policy_id(policy_scope, new_policy_id)?;
        token.accounting.emit_event(
            IB20::PolicyUpdated {
                policyScope: policy_scope,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }

    /// Approves `args.spender` on `args.owner`'s EIP-712 signature.
    pub fn permit<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &mut B20Token<S, A>,
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
        let nonce = token.accounting.nonce(args.owner)?;
        let recovered = if self.version >= B20Version::V2 {
            let signing_hash = args.metered_signing_hash(&token.accounting, domain_sep, nonce)?;
            args.metered_recover_signer(&token.accounting, signing_hash)?
        } else {
            args.recover_signer(args.signing_hash(domain_sep, nonce))?
        };
        PermitArgs::validate_recovered_address(recovered, args.owner)?;
        token.accounting.increment_nonce(args.owner)?;
        self.approve(token, args.owner, args.spender, args.value)
    }

    /// Whether `feature` is paused.
    pub fn is_paused<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &B20Token<S, A>,
        feature: IB20::PausableFeature,
    ) -> Result<bool> {
        B20PausableFeature::ensure_one_of(feature, self.pausable_features())?;
        Ok((token.accounting.paused()? & B20PausableFeature::mask(feature)) != U256::ZERO)
    }

    /// The paused features, in feature order.
    pub fn paused_features<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &B20Token<S, A>,
    ) -> Result<Vec<IB20::PausableFeature>> {
        let paused = token.accounting.paused()?;
        Ok(self
            .pausable_features()
            .iter()
            .copied()
            .filter(|feature| (paused & B20PausableFeature::mask(*feature)) != U256::ZERO)
            .collect())
    }

    /// The policy id of `policy_scope`.
    pub fn policy_id<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &B20Token<S, A>,
        policy_scope: B256,
    ) -> Result<u64> {
        self.ensure_supported_policy_type(policy_scope)?;
        token.accounting.policy_id(policy_scope)
    }

    /// The EIP-712 domain separator.
    pub fn domain_separator<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &B20Token<S, A>,
        chain_id: u64,
    ) -> Result<B256> {
        let name = token.accounting.name()?;
        let name_hash = self.keccak256(&token.accounting, name.as_bytes())?;
        let version_hash = self.keccak256(&token.accounting, Self::EIP712_VERSION)?;
        let chain_id = U256::from(chain_id);
        let address = token.accounting.token_address();
        let encoded =
            (Self::DOMAIN_TYPEHASH, name_hash, version_hash, chain_id, address).abi_encode();
        self.keccak256(&token.accounting, &encoded)
    }

    /// The EIP-5267 domain.
    pub fn eip712_domain<S: TokenAccounting, A: PolicyAccounting>(
        &self,
        token: &B20Token<S, A>,
        chain_id: u64,
    ) -> Result<Eip712Domain> {
        let name = token.accounting.name()?;
        Ok((
            // bits 0+1+2+3: name + version + chainId + verifyingContract
            FixedBytes::<1>::from([0x0f]),
            name,
            "1".to_string(),
            U256::from(chain_id),
            token.accounting.token_address(),
            B256::ZERO,
            vec![],
        ))
    }
}

#[cfg(test)]
mod tests {

    /// Version 1 (activated at Beryl).
    mod v1 {
        use alloc::{string::ToString, vec};

        use alloy_primitives::{Address, B256, U256, keccak256};
        use alloy_sol_types::SolEvent;

        use crate::BasePrecompileError;
        use k256::ecdsa::SigningKey;

        use crate::{
            B20Logic, B20PolicyType, B20Token, B20TokenRole, B20Version, FakePolicyAccounting, IB20,
            InMemoryTokenAccounting, PermitArgs, PolicyRegistryStorage, PolicyVersion,
            TokenAccounting,
        };

        const TOKEN: Address = Address::repeat_byte(0x22);
        const ADMIN: Address = Address::repeat_byte(0xAD);
        const ALICE: Address = Address::repeat_byte(0xA1);
        const BOB: Address = Address::repeat_byte(0xB0);
        const CHAIN_ID: u64 = 8453;
        const LOGIC: B20Logic = B20Logic::stablecoin(B20Version::V1);

        // Anvil/Hardhat account 0 — well-known test key, never used in production.
        const PRIVATE_KEY: [u8; 32] =
            alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

        /// Accounting seeded with the stablecoin metadata these tests assume.
        fn accounting() -> InMemoryTokenAccounting {
            let mut accounting = InMemoryTokenAccounting::new(TOKEN);
            accounting.name = "USD Coin".to_string();
            accounting.symbol = "USDC".to_string();
            accounting.currency = "USD".to_string();
            accounting
        }

        type Tok = B20Token<InMemoryTokenAccounting, FakePolicyAccounting>;

        fn token() -> Tok {
            B20Token::with_storage_and_policy(
                accounting(),
                FakePolicyAccounting::with_caller(ADMIN),
                PolicyVersion::V1,
            )
        }

        fn funded_token() -> Tok {
            let mut token = token();
            fund(&mut token, ALICE, U256::from(100u64));
            token
        }

        /// Grants `role` to `account` and keeps the admin member-count consistent.
        fn grant(tok: &mut Tok, role: B256, account: Address) {
            tok.accounting.set_role(role, account, true).unwrap();
            let next = tok.accounting.role_member_count(role).unwrap() + U256::ONE;
            tok.accounting.set_role_member_count(role, next).unwrap();
        }

        /// Credits `account` with `amount` and grows total supply to match.
        fn fund(tok: &mut Tok, account: Address, amount: U256) {
            let bal = tok.accounting.balance_of(account).unwrap();
            tok.accounting.set_balance(account, bal + amount).unwrap();
            let supply = tok.accounting.total_supply().unwrap();
            tok.accounting.set_total_supply(supply + amount).unwrap();
        }

        fn last_event_sig(tok: &Tok) -> B256 {
            tok.accounting.events.last().unwrap().topics()[0]
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
            let nonce = tok.accounting.nonce(owner).unwrap();
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
            let mut tok = funded_token();
            LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(70u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(30u64));
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
            let mut tok = funded_token();
            // ALLOWLIST with no members → sender/receiver policy checks revert.
            const POLICY: u64 = (1u64 << 56) | 7;
            tok.accounting.set_policy_id(B20PolicyType::TransferSender.id(), POLICY).unwrap();
            tok.accounting.set_policy_id(B20PolicyType::TransferReceiver.id(), POLICY).unwrap();
            tok.policy.create_existing_policy(POLICY);
            assert!(LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).is_err());
            // Authorize both parties → transfer succeeds through the guard path.
            tok.policy.allow(POLICY, ALICE);
            tok.policy.allow(POLICY, BOB);
            LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(10u64));
        }

        // --- transfer_from ---

        #[test]
        fn transfer_from_decrements_finite_allowance() {
            let mut tok = funded_token();
            tok.accounting.set_allowance(ALICE, BOB, U256::from(40u64)).unwrap();
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::from(10u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(30u64));
        }

        #[test]
        fn transfer_from_infinite_allowance_is_not_decremented() {
            let mut tok = funded_token();
            tok.accounting.set_allowance(ALICE, BOB, U256::MAX).unwrap();
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::MAX);
        }

        #[test]
        fn transfer_from_reverts_on_insufficient_allowance() {
            let mut tok = funded_token();
            tok.accounting.set_allowance(ALICE, BOB, U256::from(5u64)).unwrap();
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
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::from(50u64));
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
            tok.policy.allow(0, BOB); // MintReceiver policy is enforced even when privileged
            LOGIC.mint(&mut tok, ADMIN, BOB, U256::from(100u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(100u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::from(100u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        #[test]
        fn mint_reverts_over_supply_cap() {
            let mut tok = token();
            tok.accounting.set_supply_cap(U256::from(50u64)).unwrap();
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
            let mut tok = funded_token();
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
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::from(60u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        #[test]
        fn burn_blocked_destroys_from_unauthorized_account() {
            let mut tok = funded_token();
            // ALWAYS_BLOCK => ALICE is unauthorized/blocked; privileged skips the role check.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::TransferSender.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            LOGIC.burn_blocked(&mut tok, ADMIN, ALICE, U256::from(40u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(last_event_sig(&tok), IB20::BurnedBlocked::SIGNATURE_HASH);
        }

        #[test]
        fn burn_blocked_reverts_when_account_not_blocked() {
            let mut tok = funded_token();
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

        /// `SEIZE` is Cobalt-only. The Beryl wire rejects it at decode; this pins the matching
        /// logic-layer allowlist so V1 does not accept it via direct logic calls either.
        #[test]
        fn pause_unpause_and_is_paused_reject_seize() {
            let mut tok = token();
            let expected = BasePrecompileError::enum_conversion_error();
            assert_eq!(
                LOGIC.pause(&mut tok, ADMIN, vec![IB20::PausableFeature::SEIZE], true).unwrap_err(),
                expected
            );
            assert_eq!(
                LOGIC.unpause(&mut tok, ADMIN, vec![IB20::PausableFeature::SEIZE], true).unwrap_err(),
                expected
            );
            assert_eq!(LOGIC.is_paused(&tok, IB20::PausableFeature::SEIZE).unwrap_err(), expected);
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
            assert_eq!(tok.accounting.supply_cap().unwrap(), U256::from(1_000u64));
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
            assert_eq!(tok.accounting.name().unwrap(), "New Name");
            let events = &tok.accounting.events;
            assert_eq!(events[events.len() - 2].topics()[0], IB20::NameUpdated::SIGNATURE_HASH);
            assert_eq!(events[events.len() - 1].topics()[0], IB20::EIP712DomainChanged::SIGNATURE_HASH);
        }

        // --- roles ---

        #[test]
        fn grant_role_privileged_grants_and_emits() {
            let mut tok = token();
            LOGIC.grant_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
            assert!(tok.accounting.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
            assert_eq!(last_event_sig(&tok), IB20::RoleGranted::SIGNATURE_HASH);
        }

        #[test]
        fn revoke_role_privileged_revokes_and_emits() {
            let mut tok = token();
            grant(&mut tok, B20TokenRole::Mint.id(), ALICE);
            LOGIC.revoke_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
            assert!(!tok.accounting.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
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
            assert!(tok.accounting.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
            assert_eq!(
                tok.accounting.role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(),
                U256::ONE
            );
        }

        // --- policy ---

        #[test]
        fn update_policy_sets_new_id() {
            let mut tok = token();
            tok.policy.create_existing_policy(7);
            LOGIC.update_policy(&mut tok, ADMIN, B20PolicyType::TransferSender.id(), 7, true).unwrap();
            assert_eq!(tok.accounting.policy_id(B20PolicyType::TransferSender.id()).unwrap(), 7);
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
            assert_eq!(tok.accounting.allowance(owner, BOB).unwrap(), U256::from(500u64));
            assert_eq!(tok.accounting.nonce(owner).unwrap(), U256::ONE);
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
            assert_eq!(crate::StablecoinAccounting::currency(&tok.accounting).unwrap(), "USD");
        }

        #[test]
        fn is_initialized_reflects_storage() {
            let mut tok = token();
            assert!(tok.accounting.is_initialized().unwrap());
            tok.accounting.initialized = false;
            assert!(!tok.accounting.is_initialized().unwrap());
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

        /// Pins this version's frozen EIP-712 domain typehash to the exact type string it must hash.
        /// The constant is duplicated per version so each fork's wire surface stays independently
        /// frozen; without this check a typo in one copy would silently change that version's digest.
        #[test]
        fn domain_typehash_matches_eip712_domain_type() {
            let domain_type =
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
            assert_eq!(B20Logic::DOMAIN_TYPEHASH, keccak256(domain_type));
            assert_eq!(B20Logic::EIP712_VERSION, b"1");
        }

    }

    /// Version 2 (activated at Cobalt).
    mod v2 {
        use alloc::{string::ToString, vec, vec::Vec};

        use alloy_primitives::{Address, B256, U256, keccak256};
        use alloy_sol_types::SolEvent;

        use crate::{BasePrecompileError, Result};
        use k256::ecdsa::SigningKey;

        use crate::{
            B20Logic, B20PolicyType, B20Token, B20TokenRole, B20Version, FakePolicyAccounting, IB20,
            InMemoryTokenAccounting, PermitArgs, PolicyRegistryStorage, PolicyVersion,
            TokenAccounting,
        };

        const TOKEN: Address = Address::repeat_byte(0x22);
        const ADMIN: Address = Address::repeat_byte(0xAD);
        const ALICE: Address = Address::repeat_byte(0xA1);
        const BOB: Address = Address::repeat_byte(0xB0);
        const MEMO: B256 = B256::repeat_byte(0x77);
        const CHAIN_ID: u64 = 8453;
        const LOGIC: B20Logic = B20Logic::stablecoin(B20Version::V2);

        // Anvil/Hardhat account 0 — well-known test key, never used in production.
        const PRIVATE_KEY: [u8; 32] =
            alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

        /// Accounting seeded with the stablecoin metadata these tests assume.
        fn accounting() -> InMemoryTokenAccounting {
            let mut accounting = InMemoryTokenAccounting::new(TOKEN);
            accounting.name = "USD Coin".to_string();
            accounting.symbol = "USDC".to_string();
            accounting.currency = "USD".to_string();
            accounting
        }

        type Tok = B20Token<InMemoryTokenAccounting, FakePolicyAccounting>;

        fn token() -> Tok {
            B20Token::with_storage_and_policy(
                accounting(),
                FakePolicyAccounting::with_caller(ADMIN),
                PolicyVersion::V1,
            )
        }

        /// Grants `role` to `account` and keeps the admin member-count consistent.
        fn grant(tok: &mut Tok, role: B256, account: Address) {
            tok.accounting.set_role(role, account, true).unwrap();
            let next = tok.accounting.role_member_count(role).unwrap() + U256::ONE;
            tok.accounting.set_role_member_count(role, next).unwrap();
        }

        /// Credits `account` with `amount` and grows total supply to match.
        fn fund(tok: &mut Tok, account: Address, amount: U256) {
            let bal = tok.accounting.balance_of(account).unwrap();
            tok.accounting.set_balance(account, bal + amount).unwrap();
            let supply = tok.accounting.total_supply().unwrap();
            tok.accounting.set_total_supply(supply + amount).unwrap();
        }

        fn last_event_sig(tok: &Tok) -> B256 {
            tok.accounting.events.last().unwrap().topics()[0]
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
            let nonce = tok.accounting.nonce(owner).unwrap();
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
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(70u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(30u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        fn invalid_operation_reverts(
            balance: u64,
            operation: impl FnOnce(&mut Tok) -> Result<()>,
            expected_error: BasePrecompileError,
        ) {
            let mut token = token();
            fund(&mut token, ALICE, U256::from(balance));
            assert_eq!(operation(&mut token).unwrap_err(), expected_error);
        }

        #[test]
        fn transfer_reverts_on_zero_receiver() {
            invalid_operation_reverts(
                10,
                |tok| LOGIC.transfer(tok, ALICE, Address::ZERO, U256::from(1u64), true),
                BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO }),
            );
        }

        #[test]
        fn transfer_reverts_on_insufficient_balance() {
            invalid_operation_reverts(
                10,
                |tok| LOGIC.transfer(tok, ALICE, BOB, U256::from(50u64), true),
                BasePrecompileError::revert(IB20::InsufficientBalance {
                    sender: ALICE,
                    balance: U256::from(10u64),
                    needed: U256::from(50u64),
                }),
            );
        }

        #[test]
        fn approve_reverts_on_zero_spender() {
            invalid_operation_reverts(
                0,
                |tok| LOGIC.approve(tok, ALICE, Address::ZERO, U256::from(1u64)),
                BasePrecompileError::revert(IB20::InvalidSpender { spender: Address::ZERO }),
            );
        }

        #[test]
        fn burn_blocked_reverts_when_account_not_blocked() {
            invalid_operation_reverts(
                100,
                |tok| LOGIC.burn_blocked(tok, ADMIN, ALICE, U256::from(1u64), true),
                BasePrecompileError::revert(IB20::AccountNotBlocked { account: ALICE }),
            );
        }

        #[test]
        fn seize_reverts_on_zero_receiver() {
            invalid_operation_reverts(
                0,
                |tok| {
                    make_seizable(tok);
                    grant(tok, B20TokenRole::Seize.id(), ADMIN);
                    LOGIC.seize_with_memo(tok, ADMIN, ALICE, Address::ZERO, U256::from(1u64), MEMO)
                },
                BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO }),
            );
        }

        #[test]
        fn update_supply_cap_reverts_below_current_supply() {
            invalid_operation_reverts(
                500,
                |tok| LOGIC.update_supply_cap(tok, ADMIN, U256::from(100u64), true),
                BasePrecompileError::revert(IB20::InvalidSupplyCap {
                    currentSupply: U256::from(500u64),
                    proposedCap: U256::from(100u64),
                }),
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
            tok.accounting.set_policy_id(B20PolicyType::TransferSender.id(), POLICY).unwrap();
            tok.accounting.set_policy_id(B20PolicyType::TransferReceiver.id(), POLICY).unwrap();
            tok.policy.create_existing_policy(POLICY);
            assert!(LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).is_err());
            // Authorize both parties → transfer succeeds through the guard path.
            tok.policy.allow(POLICY, ALICE);
            tok.policy.allow(POLICY, BOB);
            LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(10u64));
        }

        // --- transfer_from ---

        #[test]
        fn transfer_from_decrements_finite_allowance() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            tok.accounting.set_allowance(ALICE, BOB, U256::from(40u64)).unwrap();
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::from(10u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(30u64));
        }

        #[test]
        fn transfer_from_infinite_allowance_is_not_decremented() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            tok.accounting.set_allowance(ALICE, BOB, U256::MAX).unwrap();
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::MAX);
        }

        #[test]
        fn transfer_from_reverts_on_insufficient_allowance() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            tok.accounting.set_allowance(ALICE, BOB, U256::from(5u64)).unwrap();
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
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::from(50u64));
            assert_eq!(last_event_sig(&tok), IB20::Approval::SIGNATURE_HASH);
        }

        // --- mint ---

        #[test]
        fn mint_privileged_increases_supply_and_balance() {
            let mut tok = token();
            tok.policy.allow(0, BOB); // MintReceiver policy is enforced even when privileged
            LOGIC.mint(&mut tok, ADMIN, BOB, U256::from(100u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(100u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::from(100u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        #[test]
        fn mint_reverts_over_supply_cap() {
            let mut tok = token();
            tok.accounting.set_supply_cap(U256::from(50u64)).unwrap();
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
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::from(60u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        #[test]
        fn burn_blocked_destroys_from_unauthorized_account() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            // ALWAYS_BLOCK => ALICE is unauthorized/blocked; privileged skips the role check.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::TransferSender.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            LOGIC.burn_blocked(&mut tok, ADMIN, ALICE, U256::from(40u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(last_event_sig(&tok), IB20::BurnedBlocked::SIGNATURE_HASH);
        }

        // --- seize ---

        /// Points `SEIZE_EXEMPT_POLICY` at the always-block policy, making every account seizable.
        fn make_seizable(tok: &mut Tok) {
            tok.accounting
                .set_policy_id(B20PolicyType::SeizeExempt.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID)
                .unwrap();
        }

        fn event_sigs(tok: &Tok) -> Vec<B256> {
            tok.accounting.events.iter().map(|e| e.topics()[0]).collect()
        }

        #[test]
        fn seize_moves_balance_and_emits_transfer_memo_event() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            let supply = tok.accounting.total_supply().unwrap();

            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(40u64), MEMO).unwrap();

            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(40u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), supply, "seize is a transfer");
            assert_eq!(
                event_sigs(&tok),
                vec![
                    IB20::Transfer::SIGNATURE_HASH,
                    IB20::Memo::SIGNATURE_HASH,
                    IB20::Seized::SIGNATURE_HASH,
                ]
            );
        }

        #[test]
        fn seize_reverts_when_account_not_seizable() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotSeizable { account: ALICE }));
        }

        #[test]
        fn seize_reverts_on_zero_from() {
            let mut tok = token();
            // A non-default `SeizeExempt` (here ALWAYS_BLOCK via `make_seizable`) treats the zero
            // address as seizable, so without the `from != 0` guard a zero-amount seize from the zero
            // address would emit a misleading `Transfer(0x0, to, 0)` that indexers read as a mint.
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);

            let err = LOGIC
                .seize_with_memo(&mut tok, ADMIN, Address::ZERO, BOB, U256::ZERO, MEMO)
                .unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidSender { sender: Address::ZERO }));
            assert!(
                event_sigs(&tok).is_empty(),
                "no misleading Transfer/Memo/Seized on a rejected zero-from seize"
            );
        }

        #[test]
        fn seize_reverts_on_self_seize() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);

            let err = LOGIC
                .seize_with_memo(&mut tok, ADMIN, ALICE, ALICE, U256::from(1u64), MEMO)
                .unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: ALICE }));
            assert_eq!(
                tok.accounting.balance_of(ALICE).unwrap(),
                U256::from(100u64),
                "balance unchanged"
            );
            assert!(
                event_sigs(&tok).is_empty(),
                "no misleading Transfer/Memo/Seized on a rejected self-seize"
            );
        }

        #[test]
        fn seize_ignores_receiver_policy_on_to() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            tok.accounting
                .set_policy_id(
                    B20PolicyType::TransferReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(50u64), MEMO).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(50u64));
        }

        #[test]
        fn seize_reverts_when_receiver_policy_forbids() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::SeizeReceiver.id(),
                    policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                })
            );
        }

        #[test]
        fn seize_unset_receiver_policy_allows_any_destination() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            // SEIZE_RECEIVER_POLICY left unset => ALWAYS_ALLOW => any destination is allowed.
            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(50u64), MEMO).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(50u64));
        }

        #[test]
        fn seize_succeeds_with_configured_receiver_policy_allow() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_ALLOW_ID,
                )
                .unwrap();
            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(50u64), MEMO).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(50u64));
        }

        #[test]
        fn seize_exempt_policy_beats_receiver_policy() {
            let mut tok = token();
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            // SEIZE_EXEMPT unset => ALICE not seizable; SEIZE_RECEIVER blocks BOB. Exempt check fires first.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotSeizable { account: ALICE }));
        }

        #[test]
        fn seize_receiver_policy_beats_balance() {
            let mut tok = token();
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            // ALICE is seizable and has zero balance, but the receiver policy forbids BOB first.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::SeizeReceiver.id(),
                    policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                })
            );
        }

        #[test]
        fn seize_requires_role() {
            let mut tok = token();
            make_seizable(&mut tok);
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                    account: ADMIN,
                    neededRole: B20TokenRole::Seize.id(),
                })
            );
        }

        #[test]
        fn seize_reverts_when_seize_paused() {
            let mut tok = token();
            make_seizable(&mut tok);
            LOGIC.pause(&mut tok, ADMIN, vec![IB20::PausableFeature::SEIZE], true).unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::ContractPaused {
                    feature: IB20::PausableFeature::SEIZE
                })
            );
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
                    vec![
                        IB20::PausableFeature::MINT,
                        IB20::PausableFeature::BURN,
                        IB20::PausableFeature::SEIZE,
                    ],
                    true,
                )
                .unwrap();
            let features = LOGIC.paused_features(&tok).unwrap();
            assert_eq!(features.len(), 3);
            assert!(features.contains(&IB20::PausableFeature::SEIZE));
            assert!(LOGIC.is_paused(&tok, IB20::PausableFeature::SEIZE).unwrap());
        }

        // --- config / metadata ---

        #[test]
        fn update_supply_cap_sets_and_emits() {
            let mut tok = token();
            LOGIC.update_supply_cap(&mut tok, ADMIN, U256::from(1_000u64), true).unwrap();
            assert_eq!(tok.accounting.supply_cap().unwrap(), U256::from(1_000u64));
            assert_eq!(last_event_sig(&tok), IB20::SupplyCapUpdated::SIGNATURE_HASH);
        }

        #[test]
        fn update_name_emits_name_updated_and_domain_changed() {
            let mut tok = token();
            LOGIC.update_name(&mut tok, ADMIN, "New Name".to_string(), true).unwrap();
            assert_eq!(tok.accounting.name().unwrap(), "New Name");
            let events = &tok.accounting.events;
            assert_eq!(events[events.len() - 2].topics()[0], IB20::NameUpdated::SIGNATURE_HASH);
            assert_eq!(events[events.len() - 1].topics()[0], IB20::EIP712DomainChanged::SIGNATURE_HASH);
        }

        // --- roles ---

        #[test]
        fn grant_role_privileged_grants_and_emits() {
            let mut tok = token();
            LOGIC.grant_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
            assert!(tok.accounting.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
            assert_eq!(last_event_sig(&tok), IB20::RoleGranted::SIGNATURE_HASH);
        }

        #[test]
        fn revoke_role_privileged_revokes_and_emits() {
            let mut tok = token();
            grant(&mut tok, B20TokenRole::Mint.id(), ALICE);
            LOGIC.revoke_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
            assert!(!tok.accounting.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
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
            assert!(tok.accounting.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
            assert_eq!(
                tok.accounting.role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(),
                U256::ONE
            );
        }

        // --- policy ---

        #[test]
        fn update_policy_sets_new_id() {
            let mut tok = token();
            tok.policy.create_existing_policy(7);
            LOGIC.update_policy(&mut tok, ADMIN, B20PolicyType::TransferSender.id(), 7, true).unwrap();
            assert_eq!(tok.accounting.policy_id(B20PolicyType::TransferSender.id()).unwrap(), 7);
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
            assert_eq!(tok.accounting.allowance(owner, BOB).unwrap(), U256::from(500u64));
            assert_eq!(tok.accounting.nonce(owner).unwrap(), U256::ONE);
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

        #[test]
        fn permit_charges_metered_hashing_and_fixed_recovery() {
            let mut tok = token();
            let owner = anvil_owner();
            let args = signed_permit(&tok, owner, BOB, U256::from(500u64), U256::MAX);

            // `signed_permit` computes the domain separator during setup, so measure deltas.
            let keccak_before = tok.accounting.keccak_charges.get();
            let gas_before = tok.accounting.gas_deducted.get();

            LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args).unwrap();

            // domain separator (name, version, encoded) + signing digest (struct hash, digest) = 5.
            assert_eq!(tok.accounting.keccak_charges.get() - keccak_before, 5);
            // Fixed ECRECOVER-comparable recovery charge, applied exactly once.
            assert_eq!(tok.accounting.gas_deducted.get() - gas_before, PermitArgs::RECOVER_GAS);
        }

        #[test]
        fn caught_revert_permit_loop_charges_each_iteration() {
            let mut tok = token();
            let owner = anvil_owner();
            // Realistic DoS shape: a valid, recoverable signature but an unrelated `owner`, so every
            // call performs the full EIP-712 hashing and secp256k1 recovery and then reverts
            // `InvalidSigner` (before the nonce is bumped, so the same args replays each iteration).
            let mut args = signed_permit(&tok, owner, BOB, U256::from(1u64), U256::MAX);
            args.owner = Address::repeat_byte(0xcc);

            const ITERATIONS: u64 = 8;
            let keccak_before = tok.accounting.keccak_charges.get();
            let gas_before = tok.accounting.gas_deducted.get();
            for _ in 0..ITERATIONS {
                assert!(LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args.clone()).is_err());
            }

            // A caught-revert loop no longer runs for free: hashing and recovery metering scale
            // linearly with the number of calls.
            assert_eq!(tok.accounting.keccak_charges.get() - keccak_before, 5 * ITERATIONS);
            assert_eq!(
                tok.accounting.gas_deducted.get() - gas_before,
                PermitArgs::RECOVER_GAS * ITERATIONS
            );
        }

        // --- reads ---

        #[test]
        fn currency_reads_storage() {
            let tok = token();
            assert_eq!(crate::StablecoinAccounting::currency(&tok.accounting).unwrap(), "USD");
        }

        #[test]
        fn is_initialized_reflects_storage() {
            let mut tok = token();
            assert!(tok.accounting.is_initialized().unwrap());
            tok.accounting.initialized = false;
            assert!(!tok.accounting.is_initialized().unwrap());
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

        /// Pins this version's frozen EIP-712 domain typehash to the exact type string it must hash.
        /// The constant is duplicated per version so each fork's wire surface stays independently
        /// frozen; without this check a typo in one copy would silently change that version's digest.
        #[test]
        fn domain_typehash_matches_eip712_domain_type() {
            let domain_type =
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
            assert_eq!(B20Logic::DOMAIN_TYPEHASH, keccak256(domain_type));
            assert_eq!(B20Logic::EIP712_VERSION, b"1");
        }

    }

    /// Version 3 (activated at Denim).
    mod v3 {
        use alloc::{string::ToString, vec, vec::Vec};

        use alloy_primitives::{Address, B256, U256, keccak256};
        use alloy_sol_types::SolEvent;

        use crate::BasePrecompileError;
        use k256::ecdsa::SigningKey;

        use crate::{
            B20Logic, B20PolicyType, B20Token, B20TokenRole, B20Variant, B20Version,
            FakePolicyAccounting, IB20, InMemoryTokenAccounting, PermitArgs, PolicyRegistryStorage,
            PolicyVersion, TokenAccounting,
        };

        const TOKEN: Address = Address::repeat_byte(0x22);
        const ADMIN: Address = Address::repeat_byte(0xAD);
        const ALICE: Address = Address::repeat_byte(0xA1);
        const BOB: Address = Address::repeat_byte(0xB0);
        const MEMO: B256 = B256::repeat_byte(0x77);
        const CHAIN_ID: u64 = 8453;
        const LOGIC: B20Logic = B20Logic::stablecoin(B20Version::V3);

        // Anvil/Hardhat account 0 — well-known test key, never used in production.
        const PRIVATE_KEY: [u8; 32] =
            alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

        /// Accounting seeded with the stablecoin metadata these tests assume.
        fn accounting() -> InMemoryTokenAccounting {
            let mut accounting = InMemoryTokenAccounting::new(TOKEN);
            accounting.name = "USD Coin".to_string();
            accounting.symbol = "USDC".to_string();
            accounting.currency = "USD".to_string();
            accounting
        }

        type Tok = B20Token<InMemoryTokenAccounting, FakePolicyAccounting>;

        fn token() -> Tok {
            B20Token::with_storage_and_policy(
                accounting(),
                FakePolicyAccounting::with_caller(ADMIN),
                PolicyVersion::V1,
            )
        }

        /// Grants `role` to `account` and keeps the admin member-count consistent.
        fn grant(tok: &mut Tok, role: B256, account: Address) {
            tok.accounting.set_role(role, account, true).unwrap();
            let next = tok.accounting.role_member_count(role).unwrap() + U256::ONE;
            tok.accounting.set_role_member_count(role, next).unwrap();
        }

        /// Credits `account` with `amount` and grows total supply to match.
        fn fund(tok: &mut Tok, account: Address, amount: U256) {
            let bal = tok.accounting.balance_of(account).unwrap();
            tok.accounting.set_balance(account, bal + amount).unwrap();
            let supply = tok.accounting.total_supply().unwrap();
            tok.accounting.set_total_supply(supply + amount).unwrap();
        }

        /// Returns a B-20 address other than [`TOKEN`].
        fn other_b20_address() -> Address {
            B20Variant::compute_address_for_discriminant(ALICE, u8::MAX, B256::repeat_byte(0x42)).0
        }

        fn last_event_sig(tok: &Tok) -> B256 {
            tok.accounting.events.last().unwrap().topics()[0]
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
            let nonce = tok.accounting.nonce(owner).unwrap();
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
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(70u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(30u64));
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
        fn transfer_reverts_on_token_receiver() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(10u64));

            let err = LOGIC.transfer(&mut tok, ALICE, TOKEN, U256::from(1u64), true).unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: TOKEN }));
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(10u64));
        }

        #[test]
        fn transfer_allows_another_b20_receiver() {
            let mut tok = token();
            let receiver = other_b20_address();
            fund(&mut tok, ALICE, U256::from(10u64));

            LOGIC.transfer(&mut tok, ALICE, receiver, U256::from(1u64), true).unwrap();

            assert_eq!(tok.accounting.balance_of(receiver).unwrap(), U256::ONE);
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
            tok.accounting.set_policy_id(B20PolicyType::TransferSender.id(), POLICY).unwrap();
            tok.accounting.set_policy_id(B20PolicyType::TransferReceiver.id(), POLICY).unwrap();
            tok.policy.create_existing_policy(POLICY);
            assert!(LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).is_err());
            // Authorize both parties → transfer succeeds through the guard path.
            tok.policy.allow(POLICY, ALICE);
            tok.policy.allow(POLICY, BOB);
            LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10u64), false).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(10u64));
        }

        // Executor, then sender, then receiver. Each call allows the account the previous gate
        // checks, so the next PolicyForbids names the next gate.
        #[test]
        fn transfer_policy_order_executor_then_sender_then_receiver() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100));

            // Counters 0 and 1 are reserved for ALWAYS_ALLOW / ALWAYS_BLOCK sentinels.
            const EXECUTOR_POLICY: u64 = (1u64 << 56) | 3;
            const SENDER_POLICY: u64 = (1u64 << 56) | 4;
            const RECEIVER_POLICY: u64 = (1u64 << 56) | 5;

            tok.accounting
                .set_policy_id(B20PolicyType::TransferExecutor.id(), EXECUTOR_POLICY)
                .unwrap();
            tok.accounting
                .set_policy_id(B20PolicyType::TransferSender.id(), SENDER_POLICY)
                .unwrap();
            tok.accounting
                .set_policy_id(B20PolicyType::TransferReceiver.id(), RECEIVER_POLICY)
                .unwrap();

            let err = LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10), false).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::TransferExecutor.id(),
                    policyId: EXECUTOR_POLICY,
                })
            );

            tok.policy.allow(EXECUTOR_POLICY, ALICE);
            let err = LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10), false).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::TransferSender.id(),
                    policyId: SENDER_POLICY,
                })
            );

            tok.policy.allow(SENDER_POLICY, ALICE);
            let err = LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(10), false).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::TransferReceiver.id(),
                    policyId: RECEIVER_POLICY,
                })
            );
        }

        // --- transfer_from ---

        #[test]
        fn transfer_from_decrements_finite_allowance() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            tok.accounting.set_allowance(ALICE, BOB, U256::from(40u64)).unwrap();
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::from(10u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(30u64));
        }

        #[test]
        fn transfer_from_infinite_allowance_is_not_decremented() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            tok.accounting.set_allowance(ALICE, BOB, U256::MAX).unwrap();
            LOGIC.transfer_from(&mut tok, BOB, ALICE, BOB, U256::from(30u64), true).unwrap();
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::MAX);
        }

        #[test]
        fn transfer_from_reverts_on_token_receiver_before_allowance() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(10u64));

            let err =
                LOGIC.transfer_from(&mut tok, BOB, ALICE, TOKEN, U256::from(1u64), true).unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: TOKEN }));
        }

        #[test]
        fn transfer_from_reverts_on_insufficient_allowance() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            tok.accounting.set_allowance(ALICE, BOB, U256::from(5u64)).unwrap();
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
            assert_eq!(tok.accounting.allowance(ALICE, BOB).unwrap(), U256::from(50u64));
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
            tok.policy.allow(0, BOB); // MintReceiver policy is enforced even when privileged
            LOGIC.mint(&mut tok, ADMIN, BOB, U256::from(100u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(100u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::from(100u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        #[test]
        fn mint_reverts_over_supply_cap() {
            let mut tok = token();
            tok.accounting.set_supply_cap(U256::from(50u64)).unwrap();
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

        #[test]
        fn mint_reverts_on_token_receiver() {
            let mut tok = token();

            let err = LOGIC.mint(&mut tok, ADMIN, TOKEN, U256::from(1u64), true).unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: TOKEN }));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::ZERO);
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
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), U256::from(60u64));
            assert_eq!(last_event_sig(&tok), IB20::Transfer::SIGNATURE_HASH);
        }

        #[test]
        fn burn_blocked_destroys_from_unauthorized_account() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            // ALWAYS_BLOCK => ALICE is unauthorized/blocked; privileged skips the role check.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::TransferSender.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            LOGIC.burn_blocked(&mut tok, ADMIN, ALICE, U256::from(40u64), true).unwrap();
            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
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

        // --- seize ---

        /// Points `SEIZE_EXEMPT_POLICY` at the always-block policy, making every account seizable.
        fn make_seizable(tok: &mut Tok) {
            tok.accounting
                .set_policy_id(B20PolicyType::SeizeExempt.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID)
                .unwrap();
        }

        fn event_sigs(tok: &Tok) -> Vec<B256> {
            tok.accounting.events.iter().map(|e| e.topics()[0]).collect()
        }

        #[test]
        fn seize_moves_balance_and_emits_transfer_memo_event() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            let supply = tok.accounting.total_supply().unwrap();

            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(40u64), MEMO).unwrap();

            assert_eq!(tok.accounting.balance_of(ALICE).unwrap(), U256::from(60u64));
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(40u64));
            assert_eq!(tok.accounting.total_supply().unwrap(), supply, "seize is a transfer");
            assert_eq!(
                event_sigs(&tok),
                vec![
                    IB20::Transfer::SIGNATURE_HASH,
                    IB20::Memo::SIGNATURE_HASH,
                    IB20::Seized::SIGNATURE_HASH,
                ]
            );
        }

        #[test]
        fn seize_reverts_when_account_not_seizable() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotSeizable { account: ALICE }));
        }

        #[test]
        fn seize_reverts_on_zero_receiver() {
            let mut tok = token();
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            let err = LOGIC
                .seize_with_memo(&mut tok, ADMIN, ALICE, Address::ZERO, U256::from(1u64), MEMO)
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO })
            );
        }

        #[test]
        fn seize_reverts_on_token_receiver_before_sender_validation() {
            let mut tok = token();
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);

            let err = LOGIC
                .seize_with_memo(&mut tok, ADMIN, Address::ZERO, TOKEN, U256::from(1u64), MEMO)
                .unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: TOKEN }));
        }

        #[test]
        fn seize_can_recover_a_balance_from_the_token_address() {
            let mut tok = token();
            fund(&mut tok, TOKEN, U256::from(10u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);

            LOGIC.seize_with_memo(&mut tok, ADMIN, TOKEN, BOB, U256::from(10u64), MEMO).unwrap();

            assert_eq!(tok.accounting.balance_of(TOKEN).unwrap(), U256::ZERO);
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(10u64));
        }

        #[test]
        fn seize_reverts_on_zero_from() {
            let mut tok = token();
            // A non-default `SeizeExempt` (here ALWAYS_BLOCK via `make_seizable`) treats the zero
            // address as seizable, so without the `from != 0` guard a zero-amount seize from the zero
            // address would emit a misleading `Transfer(0x0, to, 0)` that indexers read as a mint.
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);

            let err = LOGIC
                .seize_with_memo(&mut tok, ADMIN, Address::ZERO, BOB, U256::ZERO, MEMO)
                .unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidSender { sender: Address::ZERO }));
            assert!(
                event_sigs(&tok).is_empty(),
                "no misleading Transfer/Memo/Seized on a rejected zero-from seize"
            );
        }

        #[test]
        fn seize_reverts_on_self_seize() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(100u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);

            let err = LOGIC
                .seize_with_memo(&mut tok, ADMIN, ALICE, ALICE, U256::from(1u64), MEMO)
                .unwrap_err();

            assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: ALICE }));
            assert_eq!(
                tok.accounting.balance_of(ALICE).unwrap(),
                U256::from(100u64),
                "balance unchanged"
            );
            assert!(
                event_sigs(&tok).is_empty(),
                "no misleading Transfer/Memo/Seized on a rejected self-seize"
            );
        }

        #[test]
        fn seize_ignores_receiver_policy_on_to() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            tok.accounting
                .set_policy_id(
                    B20PolicyType::TransferReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(50u64), MEMO).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(50u64));
        }

        #[test]
        fn seize_reverts_when_receiver_policy_forbids() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::SeizeReceiver.id(),
                    policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                })
            );
        }

        #[test]
        fn seize_unset_receiver_policy_allows_any_destination() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            // SEIZE_RECEIVER_POLICY left unset => ALWAYS_ALLOW => any destination is allowed.
            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(50u64), MEMO).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(50u64));
        }

        #[test]
        fn seize_succeeds_with_configured_receiver_policy_allow() {
            let mut tok = token();
            fund(&mut tok, ALICE, U256::from(50u64));
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_ALLOW_ID,
                )
                .unwrap();
            LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(50u64), MEMO).unwrap();
            assert_eq!(tok.accounting.balance_of(BOB).unwrap(), U256::from(50u64));
        }

        #[test]
        fn seize_exempt_policy_beats_receiver_policy() {
            let mut tok = token();
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            // SEIZE_EXEMPT unset => ALICE not seizable; SEIZE_RECEIVER blocks BOB. Exempt check fires first.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotSeizable { account: ALICE }));
        }

        #[test]
        fn seize_receiver_policy_beats_balance() {
            let mut tok = token();
            make_seizable(&mut tok);
            grant(&mut tok, B20TokenRole::Seize.id(), ADMIN);
            // ALICE is seizable and has zero balance, but the receiver policy forbids BOB first.
            tok.accounting
                .set_policy_id(
                    B20PolicyType::SeizeReceiver.id(),
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                )
                .unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: B20PolicyType::SeizeReceiver.id(),
                    policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                })
            );
        }

        #[test]
        fn seize_requires_role() {
            let mut tok = token();
            make_seizable(&mut tok);
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                    account: ADMIN,
                    neededRole: B20TokenRole::Seize.id(),
                })
            );
        }

        #[test]
        fn seize_reverts_when_seize_paused() {
            let mut tok = token();
            make_seizable(&mut tok);
            LOGIC.pause(&mut tok, ADMIN, vec![IB20::PausableFeature::SEIZE], true).unwrap();
            let err =
                LOGIC.seize_with_memo(&mut tok, ADMIN, ALICE, BOB, U256::from(1u64), MEMO).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IB20::ContractPaused {
                    feature: IB20::PausableFeature::SEIZE
                })
            );
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
                    vec![
                        IB20::PausableFeature::MINT,
                        IB20::PausableFeature::BURN,
                        IB20::PausableFeature::SEIZE,
                    ],
                    true,
                )
                .unwrap();
            let features = LOGIC.paused_features(&tok).unwrap();
            assert_eq!(features.len(), 3);
            assert!(features.contains(&IB20::PausableFeature::SEIZE));
            assert!(LOGIC.is_paused(&tok, IB20::PausableFeature::SEIZE).unwrap());
        }

        // --- config / metadata ---

        #[test]
        fn update_supply_cap_sets_and_emits() {
            let mut tok = token();
            LOGIC.update_supply_cap(&mut tok, ADMIN, U256::from(1_000u64), true).unwrap();
            assert_eq!(tok.accounting.supply_cap().unwrap(), U256::from(1_000u64));
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
            assert_eq!(tok.accounting.name().unwrap(), "New Name");
            let events = &tok.accounting.events;
            assert_eq!(events[events.len() - 2].topics()[0], IB20::NameUpdated::SIGNATURE_HASH);
            assert_eq!(events[events.len() - 1].topics()[0], IB20::EIP712DomainChanged::SIGNATURE_HASH);
        }

        // --- roles ---

        #[test]
        fn grant_role_privileged_grants_and_emits() {
            let mut tok = token();
            LOGIC.grant_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
            assert!(tok.accounting.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
            assert_eq!(last_event_sig(&tok), IB20::RoleGranted::SIGNATURE_HASH);
        }

        #[test]
        fn revoke_role_privileged_revokes_and_emits() {
            let mut tok = token();
            grant(&mut tok, B20TokenRole::Mint.id(), ALICE);
            LOGIC.revoke_role(&mut tok, ADMIN, B20TokenRole::Mint.id(), ALICE, true).unwrap();
            assert!(!tok.accounting.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
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
            assert!(tok.accounting.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
            assert_eq!(
                tok.accounting.role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(),
                U256::ONE
            );
        }

        // --- policy ---

        #[test]
        fn update_policy_sets_new_id() {
            let mut tok = token();
            tok.policy.create_existing_policy(7);
            LOGIC.update_policy(&mut tok, ADMIN, B20PolicyType::TransferSender.id(), 7, true).unwrap();
            assert_eq!(tok.accounting.policy_id(B20PolicyType::TransferSender.id()).unwrap(), 7);
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
            assert_eq!(tok.accounting.allowance(owner, BOB).unwrap(), U256::from(500u64));
            assert_eq!(tok.accounting.nonce(owner).unwrap(), U256::ONE);
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

        #[test]
        fn permit_charges_metered_hashing_and_fixed_recovery() {
            let mut tok = token();
            let owner = anvil_owner();
            let args = signed_permit(&tok, owner, BOB, U256::from(500u64), U256::MAX);

            // `signed_permit` computes the domain separator during setup, so measure deltas.
            let keccak_before = tok.accounting.keccak_charges.get();
            let gas_before = tok.accounting.gas_deducted.get();

            LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args).unwrap();

            // domain separator (name, version, encoded) + signing digest (struct hash, digest) = 5.
            assert_eq!(tok.accounting.keccak_charges.get() - keccak_before, 5);
            // Fixed ECRECOVER-comparable recovery charge, applied exactly once.
            assert_eq!(tok.accounting.gas_deducted.get() - gas_before, PermitArgs::RECOVER_GAS);
        }

        #[test]
        fn caught_revert_permit_loop_charges_each_iteration() {
            let mut tok = token();
            let owner = anvil_owner();
            // Realistic DoS shape: a valid, recoverable signature but an unrelated `owner`, so every
            // call performs the full EIP-712 hashing and secp256k1 recovery and then reverts
            // `InvalidSigner` (before the nonce is bumped, so the same args replays each iteration).
            let mut args = signed_permit(&tok, owner, BOB, U256::from(1u64), U256::MAX);
            args.owner = Address::repeat_byte(0xcc);

            const ITERATIONS: u64 = 8;
            let keccak_before = tok.accounting.keccak_charges.get();
            let gas_before = tok.accounting.gas_deducted.get();
            for _ in 0..ITERATIONS {
                assert!(LOGIC.permit(&mut tok, CHAIN_ID, U256::ZERO, args.clone()).is_err());
            }

            // A caught-revert loop no longer runs for free: hashing and recovery metering scale
            // linearly with the number of calls.
            assert_eq!(tok.accounting.keccak_charges.get() - keccak_before, 5 * ITERATIONS);
            assert_eq!(
                tok.accounting.gas_deducted.get() - gas_before,
                PermitArgs::RECOVER_GAS * ITERATIONS
            );
        }

        // --- reads ---

        #[test]
        fn currency_reads_storage() {
            let tok = token();
            assert_eq!(crate::StablecoinAccounting::currency(&tok.accounting).unwrap(), "USD");
        }

        #[test]
        fn is_initialized_reflects_storage() {
            let mut tok = token();
            assert!(tok.accounting.is_initialized().unwrap());
            tok.accounting.initialized = false;
            assert!(!tok.accounting.is_initialized().unwrap());
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

        /// Pins this version's frozen EIP-712 domain typehash to the exact type string it must hash.
        /// The constant is duplicated per version so each fork's wire surface stays independently
        /// frozen; without this check a typo in one copy would silently change that version's digest.
        #[test]
        fn domain_typehash_matches_eip712_domain_type() {
            let domain_type =
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
            assert_eq!(B20Logic::DOMAIN_TYPEHASH, keccak256(domain_type));
            assert_eq!(B20Logic::EIP712_VERSION, b"1");
        }

    }

}
