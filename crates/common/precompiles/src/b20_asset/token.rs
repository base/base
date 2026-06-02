//! `B20AssetToken` struct — the asset B-20 token type.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256, b256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    AssetAccounting, B20AssetStorage, B20Guards, B20PolicyType, B20TokenRole, Burnable,
    Configurable,
    IB20::{self},
    IB20Asset, Mintable, Pausable, Permittable, Policy, RoleManaged, Token, Transferable,
};

/// EVM precompile for the asset B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S: AssetAccounting`
/// so the dispatch layer can read and write asset-specific storage (multiplier,
/// asset metadata, announcement IDs). The `in_announcement` flag guards against
/// recursive `announce` calls within a single precompile invocation.
#[derive(Debug, Clone)]
pub struct B20AssetToken<S: AssetAccounting, P: Policy> {
    accounting: S,
    policy: P,
    in_announcement: bool,
}

impl<S: AssetAccounting, P: Policy> B20AssetToken<S, P> {
    /// Role identifier for asset operators: `keccak256("OPERATOR_ROLE")`.
    pub const OPERATOR_ROLE: B256 =
        b256!("97667070c54ef182b0f5858b034beac1b6f3089aa2d3188bb1e8929f4fa9b929");

    /// Role identifier for metadata updaters: `keccak256("METADATA_ROLE")`.
    pub const METADATA_ROLE: B256 =
        b256!("6bd6b5318a46e5fff572d5e4258a20774aab40cc35ac7680654b9081fcc82f80");

    /// Policy scope identifier for redeem senders: `keccak256("REDEEM_SENDER_POLICY")`.
    pub const REDEEM_SENDER_POLICY: B256 = B20AssetStorage::REDEEM_SENDER_POLICY;

    /// Creates a `B20AssetToken` backed by the provided storage and policy adapters.
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

impl<S: AssetAccounting, P: Policy> Token for B20AssetToken<S, P> {
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

impl<S: AssetAccounting, P: Policy> Transferable for B20AssetToken<S, P> {}
impl<S: AssetAccounting, P: Policy> Mintable for B20AssetToken<S, P> {}
impl<S: AssetAccounting, P: Policy> Burnable for B20AssetToken<S, P> {}
impl<S: AssetAccounting, P: Policy> Pausable for B20AssetToken<S, P> {}
impl<S: AssetAccounting, P: Policy> Configurable for B20AssetToken<S, P> {}
impl<S: AssetAccounting, P: Policy> Permittable for B20AssetToken<S, P> {}
impl<S: AssetAccounting, P: Policy> RoleManaged for B20AssetToken<S, P> {}

// --- Asset-Specific Operations ---

impl<S: AssetAccounting, P: Policy> B20AssetToken<S, P> {
    // --- Policy Scope Validation ---

    /// Ensures `policy_scope` names either an inherited B-20 policy slot or the
    /// asset redeem slot.
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

    /// Ensures the caller has the asset operator role.
    pub fn ensure_operator_role(&self, caller: Address, privileged: bool) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, Self::OPERATOR_ROLE) }
    }

    /// Ensures the caller has the metadata role.
    pub fn ensure_metadata_role(&self, caller: Address, privileged: bool) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, Self::METADATA_ROLE) }
    }

    /// Ensures the caller has the default admin role.
    pub fn ensure_default_admin(&self, caller: Address, privileged: bool) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, Self::default_admin_role()) }
    }

    // --- Policy Scope Helpers ---

    /// Policy slot checked against transfer senders.
    pub const fn transfer_sender_policy() -> B256 {
        B20PolicyType::TransferSender.id()
    }

    /// Policy slot checked against transfer receivers.
    pub const fn transfer_receiver_policy() -> B256 {
        B20PolicyType::TransferReceiver.id()
    }

    /// Policy slot checked against delegated transfer executors.
    pub const fn transfer_executor_policy() -> B256 {
        B20PolicyType::TransferExecutor.id()
    }

    /// Policy slot checked against mint receivers.
    pub const fn mint_receiver_policy() -> B256 {
        B20PolicyType::MintReceiver.id()
    }

    // --- Policy Operations ---

    /// Returns the configured policy ID for `policy_scope`.
    pub fn policy_id(&self, policy_scope: B256) -> Result<u64> {
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

    // --- Multiplier Operations ---

    /// Converts a raw balance to its scaled view: `rawBalance * multiplier / WAD`.
    pub fn to_scaled_balance(&self, raw_balance: U256) -> Result<U256> {
        let m = self.accounting().multiplier()?;
        let product = raw_balance.checked_mul(m).ok_or_else(BasePrecompileError::under_overflow)?;
        Ok(product / B20AssetStorage::WAD)
    }

    /// Returns the scaled balance for an account.
    pub fn scaled_balance_of(&self, account: Address) -> Result<U256> {
        let balance = self.accounting().balance_of(account)?;
        self.to_scaled_balance(balance)
    }

    /// Sets a new multiplier.
    pub fn update_multiplier(
        &mut self,
        caller: Address,
        new_multiplier: U256,
        privileged: bool,
    ) -> Result<()> {
        self.ensure_operator_role(caller, privileged)?;
        self.accounting_mut().set_multiplier(new_multiplier)?;
        self.accounting_mut().emit_event(
            IB20Asset::MultiplierUpdated { multiplier: new_multiplier }.encode_log_data(),
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
            IB20Asset::MinimumRedeemableUpdated { caller, newMinimumRedeemable: new_minimum }
                .encode_log_data(),
        )
    }

    // --- Asset Metadata Operations ---

    /// Updates an asset metadata value.
    pub fn update_extra_metadata(
        &mut self,
        caller: Address,
        identifier_type: String,
        value: String,
        privileged: bool,
    ) -> Result<()> {
        self.ensure_metadata_role(caller, privileged)?;
        if identifier_type.is_empty() {
            return Err(BasePrecompileError::revert(IB20Asset::InvalidIdentifierType {}));
        }
        self.accounting_mut().set_extra_metadata_value(identifier_type.as_str(), value.clone())?;
        self.accounting_mut().emit_event(
            IB20Asset::ExtraMetadataUpdated { identifierType: identifier_type, value }
                .encode_log_data(),
        )
    }

    // --- Asset Redeem Operations ---

    /// Performs an asset-specific redeem: multiplier floor check, burn, asset `Redeemed` event.
    pub fn asset_redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        let multiplier = self.asset_redeem_burn(caller, amount)?;
        self.emit_redeemed(caller, amount, multiplier)
    }

    /// [`Self::asset_redeem`] with a memo emitted between `Transfer` and `Redeemed`.
    pub fn asset_redeem_with_memo(
        &mut self,
        caller: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        let multiplier = self.asset_redeem_burn(caller, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())?;
        self.emit_redeemed(caller, amount, multiplier)
    }

    /// Performs the shared asset redeem burn and returns the multiplier used for the floor check.
    fn asset_redeem_burn(&mut self, caller: Address, amount: U256) -> Result<U256> {
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::REDEEM)?;
        B20Guards::ensure_policy::<Self>(self, Self::REDEEM_SENDER_POLICY, caller)?;
        let multiplier = self.accounting().multiplier()?;
        if !amount.is_zero() {
            let scaled =
                amount.checked_mul(multiplier).ok_or_else(BasePrecompileError::under_overflow)?
                    / B20AssetStorage::WAD;
            let minimum = self.accounting().minimum_redeemable()?;
            if scaled == U256::ZERO || scaled < minimum {
                return Err(BasePrecompileError::revert(IB20Asset::BelowMinimumRedeemable {
                    scaledAmount: scaled,
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
        Ok(multiplier)
    }

    fn emit_redeemed(&mut self, caller: Address, amount: U256, multiplier: U256) -> Result<()> {
        self.accounting_mut().emit_event(
            IB20Asset::Redeemed { from: caller, amt: amount, multiplier }.encode_log_data(),
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
            return Err(BasePrecompileError::revert(IB20Asset::LengthMismatch {
                leftLen: U256::from(recipients.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if recipients.is_empty() {
            return Err(BasePrecompileError::revert(IB20Asset::EmptyBatch {}));
        }
        // 4. BUSINESS LOGIC (privileged=true to skip redundant pause/role checks in mint)
        for (recipient, amount) in recipients.into_iter().zip(amounts) {
            self.mint(caller, recipient, amount, true)?;
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
        B20AssetStorage, B20AssetToken, B20PausableFeature, B20TokenRole, IB20, IB20Asset,
        InMemoryPolicy, InMemoryTokenAccounting, PolicyRegistryStorage, Token,
    };

    type TestAssetToken = B20AssetToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const CALLER: Address = Address::repeat_byte(0xcc);
    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);

    fn make_token() -> TestAssetToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD;
        accounting
            .policy_ids
            .insert(TestAssetToken::REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[derive(Debug, Clone, Copy)]
    enum BatchMintSetup {
        Paused,
        NoRole,
        EmptyBatch,
        LengthMismatch,
    }

    fn setup_batch_mint(setup: BatchMintSetup) -> (TestAssetToken, Vec<Address>, Vec<U256>) {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD;
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

        let token = TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
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
            BatchMintSetup::EmptyBatch => BasePrecompileError::revert(IB20Asset::EmptyBatch {}),
            BatchMintSetup::LengthMismatch => {
                BasePrecompileError::revert(IB20Asset::LengthMismatch {
                    leftLen: U256::from(2u64),
                    rightLen: U256::from(1u64),
                })
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum AssetRedeemSetup {
        Paused,
        PolicyBlocked,
        InsufficientBalance,
    }

    fn setup_asset_redeem(setup: AssetRedeemSetup) -> TestAssetToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD;

        match setup {
            AssetRedeemSetup::Paused => {
                accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::REDEEM);
                accounting.policy_ids.insert(
                    TestAssetToken::REDEEM_SENDER_POLICY,
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                );
            }
            AssetRedeemSetup::PolicyBlocked => {
                accounting.policy_ids.insert(
                    TestAssetToken::REDEEM_SENDER_POLICY,
                    PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                );
            }
            AssetRedeemSetup::InsufficientBalance => {
                accounting.policy_ids.insert(
                    TestAssetToken::REDEEM_SENDER_POLICY,
                    PolicyRegistryStorage::ALWAYS_ALLOW_ID,
                );
                accounting.minimum_redeemable = U256::from(1u64);
                accounting.balances.insert(CALLER, U256::from(5u64));
            }
        }

        TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn expected_asset_redeem_error(setup: AssetRedeemSetup) -> BasePrecompileError {
        match setup {
            AssetRedeemSetup::Paused => BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::REDEEM,
            }),
            AssetRedeemSetup::PolicyBlocked => BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: TestAssetToken::REDEEM_SENDER_POLICY,
                policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            }),
            AssetRedeemSetup::InsufficientBalance => {
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

    fn setup_update_policy(setup: UpdatePolicySetup) -> (TestAssetToken, B256) {
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
        assert_eq!(TestAssetToken::OPERATOR_ROLE, keccak256("OPERATOR_ROLE"));
        assert_eq!(TestAssetToken::METADATA_ROLE, keccak256("METADATA_ROLE"));
        assert_eq!(TestAssetToken::REDEEM_SENDER_POLICY, keccak256("REDEEM_SENDER_POLICY"));
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
    #[case::paused_gets_pause_error(AssetRedeemSetup::Paused)]
    #[case::policy_blocked_gets_policy_error(AssetRedeemSetup::PolicyBlocked)]
    #[case::insufficient_balance_gets_business_error(AssetRedeemSetup::InsufficientBalance)]
    fn asset_redeem_check_order(#[case] setup: AssetRedeemSetup) {
        let mut token = setup_asset_redeem(setup);

        let err = token.asset_redeem(CALLER, U256::from(10u64)).unwrap_err();

        assert_eq!(err, expected_asset_redeem_error(setup));
    }

    #[rstest]
    #[case::no_role_gets_role_error(UpdatePolicySetup::NoRole)]
    #[case::invalid_scope_gets_input_error(UpdatePolicySetup::InvalidScope)]
    fn update_policy_check_order(#[case] setup: UpdatePolicySetup) {
        let (mut token, invalid_scope) = setup_update_policy(setup);

        let err = token.update_policy(CALLER, invalid_scope, 999, false).unwrap_err();

        assert_eq!(err, expected_update_policy_error(setup, invalid_scope));
    }

    /// Caller holding only `OPERATOR_ROLE` must be denied `updateExtraMetadata`.
    ///
    /// This guards against regressions where `ensure_metadata_role` is accidentally
    /// reverted to `ensure_operator_role`, collapsing the two distinct capabilities.
    #[test]
    fn update_extra_metadata_requires_metadata_role_not_operator_role() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((TestAssetToken::OPERATOR_ROLE, CALLER), true);

        let err = token
            .update_extra_metadata(CALLER, "ISIN".to_string(), "US0000000000".to_string(), false)
            .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: TestAssetToken::METADATA_ROLE,
            })
        );
    }
}
