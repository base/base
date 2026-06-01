//! `B20SecurityToken` struct — the security B-20 token type.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256, b256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20Guards, B20PolicyType, B20SecurityStorage, B20TokenRole, Burnable, Configurable,
    IB20::{self},
    IB20Security, Mintable, Pausable, Permittable, Policy, RoleManaged, SecurityAccounting, Token,
    Transferable,
};

/// EVM precompile for the security B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S: SecurityAccounting`
/// so the dispatch layer can read and write security-specific storage (share ratio,
/// security identifiers, announcement IDs). The `in_announcement` flag guards against
/// recursive `announce` calls within a single precompile invocation.
#[derive(Debug, Clone)]
pub struct B20SecurityToken<S: SecurityAccounting, P: Policy> {
    accounting: S,
    policy: P,
    in_announcement: bool,
}

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// Role identifier for security operators: `keccak256("SECURITY_OPERATOR_ROLE")`.
    pub const SECURITY_OPERATOR_ROLE: B256 =
        b256!("e63901dfe7775ace99fa3654743976eb0ab2009f5d19c4fc1ecd40aed27d59af");

    /// Role identifier for delegated burns: `keccak256("BURN_FROM_ROLE")`.
    pub const BURN_FROM_ROLE: B256 =
        b256!("25400dba76bf0d00acf274c2b61ff56aa4ed19826e21e0186e3fecd6a6671875");

    /// Policy scope identifier for redeem senders: `keccak256("REDEEM_SENDER_POLICY")`.
    pub const REDEEM_SENDER_POLICY: B256 = B20SecurityStorage::REDEEM_SENDER_POLICY;

    /// Creates a `B20SecurityToken` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy, in_announcement: false }
    }

    /// Returns whether this token is currently executing an announcement.
    pub const fn is_announcement_active(&self) -> bool {
        self.in_announcement
    }

    /// Marks this token as executing an announcement.
    pub const fn begin_announcement(&mut self) {
        self.in_announcement = true;
    }
}

impl<S: SecurityAccounting, P: Policy> Token for B20SecurityToken<S, P> {
    type Accounting = S;
    type Policy = P;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &P {
        &self.policy
    }

    fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}

impl<S: SecurityAccounting, P: Policy> Transferable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Mintable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Burnable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Pausable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Configurable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Permittable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> RoleManaged for B20SecurityToken<S, P> {}

// --- Security-Specific Operations ---

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    // --- Policy Scope Validation ---

    /// Ensures `policy_scope` names either an inherited B-20 policy slot or the
    /// security redeem slot.
    pub fn is_supported_policy_scope(policy_scope: B256) -> bool {
        policy_scope == Self::REDEEM_SENDER_POLICY || B20PolicyType::from_id(policy_scope).is_some()
    }

    /// Validates that the policy scope is supported.
    pub fn ensure_supported_policy_type(policy_scope: B256) -> Result<()> {
        if Self::is_supported_policy_scope(policy_scope) {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }))
        }
    }

    // --- Authorization Helpers ---

    /// Ensures the caller has the security operator role.
    pub fn ensure_security_operator(&self, caller: Address, privileged: bool) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, Self::SECURITY_OPERATOR_ROLE) }
    }

    /// Ensures the caller has the default admin role.
    pub fn ensure_default_admin(&self, caller: Address, privileged: bool) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, Self::default_admin_role()) }
    }

    /// Ensures the caller has the burn-from role.
    pub fn ensure_burn_from_role(&self, caller: Address) -> Result<()> {
        self.ensure_role(caller, Self::BURN_FROM_ROLE)
    }

    // --- Policy Operations ---

    /// Returns the configured policy ID for `policy_scope`.
    pub fn policy_id_checked(&self, policy_scope: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_scope)?;
        self.accounting().policy_id(policy_scope)
    }

    /// Updates the configured policy ID for `policy_scope`.
    pub fn update_policy(
        &mut self,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            self.ensure_role(caller, Self::default_admin_role())?;
        }
        Self::ensure_supported_policy_type(policy_scope)?;
        if !self.policy().policy_exists(new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        let old_policy_id = self.accounting().policy_id(policy_scope)?;
        self.accounting_mut().set_policy_id(policy_scope, new_policy_id)?;
        self.accounting_mut().emit_event(
            IB20::PolicyUpdated {
                policyScope: policy_scope,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }

    // --- Share Ratio Operations ---

    /// Converts a token balance to shares: `balance * sharesToTokensRatio / WAD`.
    pub fn to_shares(&self, balance: U256) -> Result<U256> {
        let ratio = self.accounting().shares_to_tokens_ratio()?;
        let product = balance.checked_mul(ratio).ok_or_else(BasePrecompileError::under_overflow)?;
        Ok(product / B20SecurityStorage::WAD)
    }

    /// Returns the shares for an account (balance converted to shares).
    pub fn shares_of(&self, account: Address) -> Result<U256> {
        let balance = self.accounting().balance_of(account)?;
        self.to_shares(balance)
    }

    /// Updates the share-to-tokens ratio.
    pub fn update_share_ratio(
        &mut self,
        caller: Address,
        new_ratio: U256,
        privileged: bool,
    ) -> Result<()> {
        self.ensure_security_operator(caller, privileged)?;
        self.accounting_mut().set_shares_to_tokens_ratio(new_ratio)?;
        self.accounting_mut().emit_event(
            IB20Security::ShareRatioUpdated { sharesToTokensRatio: new_ratio }.encode_log_data(),
        )
    }

    // --- Minimum Redeemable Operations ---

    /// Updates the minimum redeemable amount.
    pub fn update_minimum_redeemable(
        &mut self,
        caller: Address,
        new_minimum: U256,
        privileged: bool,
    ) -> Result<()> {
        self.ensure_default_admin(caller, privileged)?;
        self.accounting_mut().set_minimum_redeemable(new_minimum)?;
        self.accounting_mut().emit_event(
            IB20Security::MinimumRedeemableUpdated { caller, newMinimumRedeemable: new_minimum }
                .encode_log_data(),
        )
    }

    // --- Security Identifier Operations ---

    /// Updates a security identifier value.
    pub fn update_security_identifier(
        &mut self,
        caller: Address,
        identifier_type: String,
        value: String,
        privileged: bool,
    ) -> Result<()> {
        self.ensure_security_operator(caller, privileged)?;
        if identifier_type.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::InvalidIdentifierType {}));
        }
        self.accounting_mut()
            .set_security_identifier_value(identifier_type.as_str(), value.clone())?;
        self.accounting_mut().emit_event(
            IB20Security::SecurityIdentifierUpdated { identifierType: identifier_type, value }
                .encode_log_data(),
        )
    }

    // --- Security Redeem Operations ---

    /// Performs a security-specific redeem: share-based floor check, burn, security `Redeemed` event.
    pub fn security_redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        let ratio = self.security_redeem_burn(caller, amount)?;
        self.emit_redeemed(caller, amount, ratio)
    }

    /// [`Self::security_redeem`] with a memo emitted between `Transfer` and `Redeemed`.
    pub fn security_redeem_with_memo(
        &mut self,
        caller: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        let ratio = self.security_redeem_burn(caller, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())?;
        self.emit_redeemed(caller, amount, ratio)
    }

    /// Performs the shared security redeem burn and returns the ratio used for the floor check.
    fn security_redeem_burn(&mut self, caller: Address, amount: U256) -> Result<U256> {
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::REDEEM)?;
        B20Guards::ensure_policy::<Self>(self, Self::REDEEM_SENDER_POLICY, caller)?;
        let ratio = self.accounting().shares_to_tokens_ratio()?;
        if !amount.is_zero() {
            let shares =
                amount.checked_mul(ratio).ok_or_else(BasePrecompileError::under_overflow)?
                    / B20SecurityStorage::WAD;
            let minimum = self.accounting().minimum_redeemable()?;
            if shares == U256::ZERO || shares < minimum {
                return Err(BasePrecompileError::revert(IB20Security::BelowMinimumRedeemable {
                    shares,
                    minimum,
                }));
            }
        }
        let balance = self.accounting().balance_of(caller)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: caller,
                balance,
                needed: amount,
            }));
        }
        self.accounting_mut().set_balance(caller, balance - amount)?;
        let supply = self.accounting().total_supply()?;
        let new_supply =
            supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_total_supply(new_supply)?;
        self.accounting_mut().emit_event(
            IB20::Transfer { from: caller, to: Address::ZERO, amount }.encode_log_data(),
        )?;
        Ok(ratio)
    }

    fn emit_redeemed(&mut self, caller: Address, amount: U256, ratio: U256) -> Result<()> {
        self.accounting_mut().emit_event(
            IB20Security::Redeemed { from: caller, amt: amount, sharesToTokensRatio: ratio }
                .encode_log_data(),
        )
    }

    // --- Batch Operations ---

    /// Mints tokens to multiple recipients. All-or-nothing.
    ///
    /// Check order: PAUSE → ROLE → INPUT → BUSINESS
    pub fn batch_mint(
        &mut self,
        caller: Address,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        // 1. PAUSE (kill switch)
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::MINT)?;
        // 2. ROLE (unless privileged)
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Mint)?;
        }
        // 3. INPUT VALIDATION
        if recipients.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(recipients.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if recipients.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        // 4. BUSINESS LOGIC (privileged=true to skip redundant pause/role checks in mint)
        for (recipient, amount) in recipients.into_iter().zip(amounts) {
            self.mint(caller, recipient, amount, true)?;
        }
        Ok(())
    }

    /// Burns tokens from multiple accounts unconditionally. All-or-nothing.
    ///
    /// Unlike `burnBlocked`, this path has no policy precondition. The
    /// `BURN_FROM_ROLE` authorization and burn pause check are the only gates.
    ///
    /// Check order: PAUSE → ROLE → INPUT → BUSINESS
    pub fn batch_burn(
        &mut self,
        caller: Address,
        accounts: Vec<Address>,
        amounts: Vec<U256>,
    ) -> Result<()> {
        // 1. PAUSE (kill switch)
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::BURN)?;
        // 2. ROLE
        self.ensure_burn_from_role(caller)?;
        // 3. INPUT VALIDATION
        if accounts.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(accounts.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if accounts.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        // 4. BUSINESS LOGIC (no allowance/policy for batch_burn)
        for (account, amount) in accounts.into_iter().zip(amounts) {
            let balance = self.accounting().balance_of(account)?;
            if balance < amount {
                return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                    sender: account,
                    balance,
                    needed: amount,
                }));
            }
            self.accounting_mut().set_balance(account, balance - amount)?;
            let supply = self.accounting().total_supply()?;
            let new_supply =
                supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
            self.accounting_mut().set_total_supply(new_supply)?;
            self.accounting_mut().emit_event(
                IB20::Transfer { from: account, to: Address::ZERO, amount }.encode_log_data(),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{Address, B256, U256, keccak256};
    use base_precompile_storage::BasePrecompileError;
    use rstest::rstest;

    use crate::{
        B20PausableFeature, B20SecurityStorage, B20SecurityToken, B20TokenRole, IB20, IB20Security,
        InMemoryPolicy, InMemoryTokenAccounting, PolicyRegistryStorage, Token,
    };

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const CALLER: Address = Address::repeat_byte(0xcc);
    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);

    fn make_token() -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = B20SecurityStorage::WAD;
        accounting.policy_ids.insert(
            TestSecurityToken::REDEEM_SENDER_POLICY,
            PolicyRegistryStorage::ALWAYS_ALLOW_ID,
        );
        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[derive(Debug, Clone, Copy)]
    enum BatchMintSetup {
        Paused,
        NoRole,
        EmptyBatch,
        LengthMismatch,
    }

    fn setup_batch_mint(setup: BatchMintSetup) -> (TestSecurityToken, Vec<Address>, Vec<U256>) {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = B20SecurityStorage::WAD;
        let recipients;
        let amounts;

        match setup {
            BatchMintSetup::Paused => {
                accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::MINT);
                recipients = vec![ALICE, BOB];
                amounts = vec![U256::from(10u64)];
            }
            BatchMintSetup::NoRole => {
                recipients = vec![];
                amounts = vec![];
            }
            BatchMintSetup::EmptyBatch => {
                accounting.roles.insert((B20TokenRole::Mint.id(), CALLER), true);
                recipients = vec![];
                amounts = vec![];
            }
            BatchMintSetup::LengthMismatch => {
                accounting.roles.insert((B20TokenRole::Mint.id(), CALLER), true);
                recipients = vec![ALICE, BOB];
                amounts = vec![U256::from(10u64)];
            }
        }

        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        (token, recipients, amounts)
    }

    fn expected_batch_mint_error(setup: BatchMintSetup) -> BasePrecompileError {
        match setup {
            BatchMintSetup::Paused => BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::MINT,
            }),
            BatchMintSetup::NoRole => {
                BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                    account: CALLER,
                    neededRole: B20TokenRole::Mint.id(),
                })
            }
            BatchMintSetup::EmptyBatch => BasePrecompileError::revert(IB20Security::EmptyBatch {}),
            BatchMintSetup::LengthMismatch => {
                BasePrecompileError::revert(IB20Security::LengthMismatch {
                    leftLen: U256::from(2u64),
                    rightLen: U256::from(1u64),
                })
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum BatchBurnSetup {
        Paused,
        NoRole,
        EmptyBatch,
        LengthMismatch,
        InsufficientBalance,
    }

    fn setup_batch_burn(setup: BatchBurnSetup) -> (TestSecurityToken, Vec<Address>, Vec<U256>) {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = B20SecurityStorage::WAD;
        let accounts;
        let amounts;

        match setup {
            BatchBurnSetup::Paused => {
                accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::BURN);
                accounts = vec![ALICE];
                amounts = vec![U256::from(10u64)];
            }
            BatchBurnSetup::NoRole => {
                accounts = vec![];
                amounts = vec![];
            }
            BatchBurnSetup::EmptyBatch => {
                accounting.roles.insert((TestSecurityToken::BURN_FROM_ROLE, CALLER), true);
                accounts = vec![];
                amounts = vec![];
            }
            BatchBurnSetup::LengthMismatch => {
                accounting.roles.insert((TestSecurityToken::BURN_FROM_ROLE, CALLER), true);
                accounts = vec![ALICE, BOB];
                amounts = vec![U256::from(10u64)];
            }
            BatchBurnSetup::InsufficientBalance => {
                accounting.roles.insert((TestSecurityToken::BURN_FROM_ROLE, CALLER), true);
                accounting.balances.insert(ALICE, U256::from(5u64));
                accounts = vec![ALICE];
                amounts = vec![U256::from(10u64)];
            }
        }

        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        (token, accounts, amounts)
    }

    fn expected_batch_burn_error(setup: BatchBurnSetup) -> BasePrecompileError {
        match setup {
            BatchBurnSetup::Paused => BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::BURN,
            }),
            BatchBurnSetup::NoRole => {
                BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                    account: CALLER,
                    neededRole: TestSecurityToken::BURN_FROM_ROLE,
                })
            }
            BatchBurnSetup::EmptyBatch => BasePrecompileError::revert(IB20Security::EmptyBatch {}),
            BatchBurnSetup::LengthMismatch => {
                BasePrecompileError::revert(IB20Security::LengthMismatch {
                    leftLen: U256::from(2u64),
                    rightLen: U256::from(1u64),
                })
            }
            BatchBurnSetup::InsufficientBalance => {
                BasePrecompileError::revert(IB20::InsufficientBalance {
                    sender: ALICE,
                    balance: U256::from(5u64),
                    needed: U256::from(10u64),
                })
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum SecurityRedeemSetup {
        Paused,
        PolicyBlocked,
        InsufficientBalance,
    }

    fn setup_security_redeem(setup: SecurityRedeemSetup) -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = B20SecurityStorage::WAD;

        match setup {
            SecurityRedeemSetup::Paused => {
                accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::REDEEM);
                accounting.policy_ids.insert(
                    TestSecurityToken::REDEEM_SENDER_POLICY,
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                );
            }
            SecurityRedeemSetup::PolicyBlocked => {
                accounting.policy_ids.insert(
                    TestSecurityToken::REDEEM_SENDER_POLICY,
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                );
            }
            SecurityRedeemSetup::InsufficientBalance => {
                accounting.policy_ids.insert(
                    TestSecurityToken::REDEEM_SENDER_POLICY,
                    PolicyRegistryStorage::ALWAYS_ALLOW_ID,
                );
                accounting.minimum_redeemable = U256::from(1u64);
                accounting.balances.insert(CALLER, U256::from(5u64));
            }
        }

        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn expected_security_redeem_error(setup: SecurityRedeemSetup) -> BasePrecompileError {
        match setup {
            SecurityRedeemSetup::Paused => BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::REDEEM,
            }),
            SecurityRedeemSetup::PolicyBlocked => {
                BasePrecompileError::revert(IB20::PolicyForbids {
                    policyScope: TestSecurityToken::REDEEM_SENDER_POLICY,
                    policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                })
            }
            SecurityRedeemSetup::InsufficientBalance => {
                BasePrecompileError::revert(IB20::InsufficientBalance {
                    sender: CALLER,
                    balance: U256::from(5u64),
                    needed: U256::from(10u64),
                })
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum UpdatePolicySetup {
        NoRole,
        InvalidScope,
    }

    fn setup_update_policy(setup: UpdatePolicySetup) -> (TestSecurityToken, B256) {
        let mut token = make_token();
        let invalid_scope = B256::repeat_byte(0xff);

        if let UpdatePolicySetup::InvalidScope = setup {
            token.accounting_mut().roles.insert((B20TokenRole::DefaultAdmin.id(), CALLER), true);
        }

        (token, invalid_scope)
    }

    fn expected_update_policy_error(setup: UpdatePolicySetup, scope: B256) -> BasePrecompileError {
        match setup {
            UpdatePolicySetup::NoRole => {
                BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                    account: CALLER,
                    neededRole: B20TokenRole::DefaultAdmin.id(),
                })
            }
            UpdatePolicySetup::InvalidScope => {
                BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: scope })
            }
        }
    }

    #[test]
    fn role_and_policy_ids_match_solidity_hashes() {
        assert_eq!(TestSecurityToken::SECURITY_OPERATOR_ROLE, keccak256("SECURITY_OPERATOR_ROLE"));
        assert_eq!(TestSecurityToken::BURN_FROM_ROLE, keccak256("BURN_FROM_ROLE"));
        assert_eq!(TestSecurityToken::REDEEM_SENDER_POLICY, keccak256("REDEEM_SENDER_POLICY"));
    }

    #[rstest]
    #[case::paused_gets_pause_error(BatchMintSetup::Paused)]
    #[case::no_role_gets_role_error(BatchMintSetup::NoRole)]
    #[case::empty_batch_gets_input_error(BatchMintSetup::EmptyBatch)]
    #[case::length_mismatch_gets_input_error(BatchMintSetup::LengthMismatch)]
    fn batch_mint_check_order(#[case] setup: BatchMintSetup) {
        let (mut token, recipients, amounts) = setup_batch_mint(setup);

        let err = token.batch_mint(CALLER, recipients, amounts, false).unwrap_err();

        assert_eq!(err, expected_batch_mint_error(setup));
    }

    #[rstest]
    #[case::paused_gets_pause_error(BatchBurnSetup::Paused)]
    #[case::no_role_gets_role_error(BatchBurnSetup::NoRole)]
    #[case::empty_batch_gets_input_error(BatchBurnSetup::EmptyBatch)]
    #[case::length_mismatch_gets_input_error(BatchBurnSetup::LengthMismatch)]
    #[case::insufficient_balance_gets_business_error(BatchBurnSetup::InsufficientBalance)]
    fn batch_burn_check_order(#[case] setup: BatchBurnSetup) {
        let (mut token, accounts, amounts) = setup_batch_burn(setup);

        let err = token.batch_burn(CALLER, accounts, amounts).unwrap_err();

        assert_eq!(err, expected_batch_burn_error(setup));
    }

    #[rstest]
    #[case::paused_gets_pause_error(SecurityRedeemSetup::Paused)]
    #[case::policy_blocked_gets_policy_error(SecurityRedeemSetup::PolicyBlocked)]
    #[case::insufficient_balance_gets_business_error(SecurityRedeemSetup::InsufficientBalance)]
    fn security_redeem_check_order(#[case] setup: SecurityRedeemSetup) {
        let mut token = setup_security_redeem(setup);

        let err = token.security_redeem(CALLER, U256::from(10u64)).unwrap_err();

        assert_eq!(err, expected_security_redeem_error(setup));
    }

    #[rstest]
    #[case::no_role_gets_role_error(UpdatePolicySetup::NoRole)]
    #[case::invalid_scope_gets_input_error(UpdatePolicySetup::InvalidScope)]
    fn update_policy_check_order(#[case] setup: UpdatePolicySetup) {
        let (mut token, invalid_scope) = setup_update_policy(setup);

        let err = token.update_policy(CALLER, invalid_scope, 999, false).unwrap_err();

        assert_eq!(err, expected_update_policy_error(setup, invalid_scope));
    }
}
