//! ABI dispatch for the asset B-20 variant.
//!
//! Security-specific selectors are tried first via `IB20Asset::IB20AssetCalls`.
//! This catches overridden selectors (`redeem`, `redeemWithMemo`) before the
//! inherited `IB20` fallthrough, ensuring security semantics always apply.
//! The `IB20` match block still includes those arms (Rust requires exhaustiveness)
//! and routes them to the same security implementation as a safety net.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent, SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    B20AssetStorage, B20AssetToken, B20PolicyType, B20TokenRole, Burnable, Configurable,
    IB20::{self, IB20Calls as C},
    IB20Asset::{self, IB20AssetCalls as SC},
    Mintable, NoopPrecompileCallObserver, Pausable, PermitArgs, Permittable, Policy,
    PrecompileCallObserver, RoleManaged, SecurityAccounting, Token, Transferable,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl<S: SecurityAccounting, P: Policy> B20AssetToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Asset` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` and observes the decoded asset B-20 operation.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        deduct_calldata_cost!(ctx, calldata);

        match self.accounting().is_initialized() {
            Ok(true) => {}
            Ok(false) => {
                return BasePrecompileError::Revert(Bytes::new())
                    .into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
            }
            Err(e) => return e.into_precompile_result(ctx.gas_used(), ctx.state_gas_used()),
        }
        self.inner_with_observer(ctx, calldata, observer).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            |b| b,
        )
    }

    /// Decodes calldata and executes the matching `IB20Asset` or inherited `IB20` operation.
    pub fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> base_precompile_storage::Result<Bytes> {
        self.inner_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// Decodes calldata, observes the decoded operation, and executes the matching handler.
    pub fn inner_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        self.inner_with_privilege_and_observer(ctx, calldata, false, observer)
    }

    /// Decodes calldata and executes it with optional factory-init privilege.
    pub fn inner_with_privilege(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        self.inner_with_privilege_and_observer(
            ctx,
            calldata,
            privileged,
            NoopPrecompileCallObserver,
        )
    }

    /// Decodes calldata, observes the decoded operation, and executes it with optional
    /// factory-init privilege.
    pub fn inner_with_privilege_and_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        // Security-specific and overridden selectors are caught here first.
        if let Ok(call) = IB20Asset::IB20AssetCalls::abi_decode(calldata) {
            let label = call.as_label();
            return observer.observe(label, || self.handle_security_call(ctx, call, privileged));
        }

        // Fall through to inherited IB20 selectors.
        let call = decode_precompile_call!(calldata, IB20::IB20Calls);
        let label = call.as_label();

        observer.observe(label, || self.handle_b20_call(ctx, call, privileged))
    }

    fn handle_b20_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: C,
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        let encoded: Bytes = match call {
            // --- Pure reads ---
            C::name(_) => self.accounting().name()?.abi_encode().into(),
            C::symbol(_) => self.accounting().symbol()?.abi_encode().into(),
            C::decimals(_) => U256::from(self.accounting().decimals()?).abi_encode().into(),
            C::totalSupply(_) => self.accounting().total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.accounting().balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => self.accounting().allowance(c.owner, c.spender)?.abi_encode().into(),
            C::supplyCap(_) => self.accounting().supply_cap()?.abi_encode().into(),
            C::nonces(c) => self.accounting().nonce(c.owner)?.abi_encode().into(),
            C::contractURI(_) => self.accounting().contract_uri()?.abi_encode().into(),

            // --- Role identifiers ---
            C::DEFAULT_ADMIN_ROLE(_) => Self::default_admin_role().abi_encode().into(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),

            // --- Policy type identifiers ---
            C::TRANSFER_SENDER_POLICY(_) => B20PolicyType::TransferSender.id().abi_encode().into(),
            C::TRANSFER_RECEIVER_POLICY(_) => {
                B20PolicyType::TransferReceiver.id().abi_encode().into()
            }
            C::TRANSFER_EXECUTOR_POLICY(_) => {
                B20PolicyType::TransferExecutor.id().abi_encode().into()
            }
            C::MINT_RECEIVER_POLICY(_) => B20PolicyType::MintReceiver.id().abi_encode().into(),

            // --- Role reads ---
            C::hasRole(c) => self.accounting().has_role(c.role, c.account)?.abi_encode().into(),
            C::getRoleAdmin(c) => self.accounting().role_admin(c.role)?.abi_encode().into(),

            // --- Pause reads ---
            C::pausedFeatures(_) => self.paused_features()?.abi_encode().into(),
            C::isPaused(c) => self.is_paused(c.feature)?.abi_encode().into(),

            // --- Policy reads ---
            C::policyId(c) => self.policy_id_checked(c.policyScope)?.abi_encode().into(),

            // --- Domain reads ---
            C::DOMAIN_SEPARATOR(_) => self.domain_separator(ctx.chain_id())?.abi_encode().into(),
            C::eip712Domain(_) => {
                let (fields, name, version, chain_id, verifying_contract, salt, extensions) =
                    self.eip712_domain(ctx.chain_id())?;
                IB20::eip712DomainCall::abi_encode_returns(&IB20::eip712DomainReturn {
                    fields,
                    name,
                    version,
                    chainId: chain_id,
                    verifyingContract: verifying_contract,
                    salt,
                    extensions,
                })
                .into()
            }

            // --- ERC-20 mutating ---
            C::transfer(c) => {
                let caller = ctx.caller();
                self.transfer(caller, c.to, c.amount, privileged)?;
                true.abi_encode().into()
            }
            C::transferFrom(c) => {
                let caller = ctx.caller();
                self.transfer_from(caller, c.from, c.to, c.amount, privileged)?;
                true.abi_encode().into()
            }
            C::approve(c) => {
                let caller = ctx.caller();
                self.approve(caller, c.spender, c.amount)?;
                true.abi_encode().into()
            }
            C::transferWithMemo(c) => {
                let caller = ctx.caller();
                self.transfer_with_memo(caller, c.to, c.amount, c.memo, privileged)?;
                true.abi_encode().into()
            }
            C::transferFromWithMemo(c) => {
                let caller = ctx.caller();
                self.transfer_from_with_memo(caller, c.from, c.to, c.amount, c.memo, privileged)?;
                true.abi_encode().into()
            }

            // --- Mint ---
            C::mint(c) => {
                let caller = ctx.caller();
                self.mint(caller, c.to, c.amount, privileged)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                let caller = ctx.caller();
                self.mint_with_memo(caller, c.to, c.amount, c.memo, privileged)?;
                Bytes::new()
            }

            // --- Burn ---
            // Self-burn operations are never factory-privileged: during init the caller is the
            // factory, not a token holder.
            C::burn(c) => {
                let caller = ctx.caller();
                self.burn(caller, caller, c.amount, false)?;
                Bytes::new()
            }
            C::burnWithMemo(c) => {
                let caller = ctx.caller();
                self.burn_with_memo(caller, caller, c.amount, c.memo, false)?;
                Bytes::new()
            }
            C::burnBlocked(c) => {
                let caller = ctx.caller();
                self.burn_blocked(caller, c.from, c.amount, privileged)?;
                Bytes::new()
            }

            // --- Pause ---
            C::pause(c) => {
                let caller = ctx.caller();
                self.pause(caller, c.features, privileged)?;
                Bytes::new()
            }
            C::unpause(c) => {
                let caller = ctx.caller();
                self.unpause(caller, c.features, privileged)?;
                Bytes::new()
            }

            // --- Admin ---
            C::updateSupplyCap(c) => {
                let caller = ctx.caller();
                Configurable::update_supply_cap(self, caller, c.newSupplyCap, privileged)?;
                Bytes::new()
            }
            C::updateName(c) => {
                let caller = ctx.caller();
                Configurable::update_name(self, caller, c.newName, privileged)?;
                Bytes::new()
            }
            C::updateSymbol(c) => {
                let caller = ctx.caller();
                Configurable::update_symbol(self, caller, c.newSymbol, privileged)?;
                Bytes::new()
            }
            C::updateContractURI(c) => {
                let caller = ctx.caller();
                Configurable::update_contract_uri(self, caller, c.newURI, privileged)?;
                Bytes::new()
            }

            // --- Role mutations ---
            C::grantRole(c) => {
                let caller = ctx.caller();
                self.grant_role(caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            C::revokeRole(c) => {
                let caller = ctx.caller();
                self.revoke_role(caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            // Renounce operations are never factory-privileged: they are only meaningful for the
            // role holder making the call after token creation.
            C::renounceRole(c) => {
                let caller = ctx.caller();
                self.renounce_role(caller, c.role, c.callerConfirmation)?;
                Bytes::new()
            }
            C::renounceLastAdmin(_) => {
                let caller = ctx.caller();
                self.renounce_last_admin(caller)?;
                Bytes::new()
            }
            C::setRoleAdmin(c) => {
                let caller = ctx.caller();
                self.set_role_admin(caller, c.role, c.newAdminRole, privileged)?;
                Bytes::new()
            }

            // --- Policy mutations ---
            C::updatePolicy(c) => {
                let caller = ctx.caller();
                self.update_policy(caller, c.policyScope, c.newPolicyId, privileged)?;
                Bytes::new()
            }

            // --- Permit ---
            C::permit(c) => {
                self.permit(
                    ctx.chain_id(),
                    ctx.timestamp(),
                    PermitArgs {
                        owner: c.owner,
                        spender: c.spender,
                        value: c.value,
                        deadline: c.deadline,
                        v: c.v,
                        r: c.r,
                        s: c.s,
                    },
                )?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }

    fn handle_security_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: SC,
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        let caller = ctx.caller();
        let encoded: Bytes = match call {
            // --- Role / precision constants ---
            SC::OPERATOR_ROLE(_) => Self::OPERATOR_ROLE.abi_encode().into(),
            SC::WAD_PRECISION(_) => B20AssetStorage::WAD.abi_encode().into(),
            SC::REDEEM_SENDER_POLICY(_) => Self::REDEEM_SENDER_POLICY.abi_encode().into(),

            // --- Multiplier reads ---
            SC::multiplier(_) => self.accounting().multiplier()?.abi_encode().into(),
            SC::toScaledBalance(c) => self.to_scaled_balance(c.rawBalance)?.abi_encode().into(),
            SC::scaledBalanceOf(c) => self.scaled_balance_of(c.account)?.abi_encode().into(),

            // --- Announcement reads ---
            SC::isAnnouncementIdUsed(c) => {
                self.accounting().is_announcement_id_used(c.id.as_str())?.abi_encode().into()
            }

            // --- Security identifier reads ---
            SC::extraMetadata(c) => {
                self.accounting().extra_metadata(c.identifierType.as_str())?.abi_encode().into()
            }

            // --- Multiplier mutations ---
            SC::updateMultiplier(c) => {
                self.update_multiplier(caller, c.newMultiplier, privileged)?;
                Bytes::new()
            }

            // --- Announcement ---
            SC::announce(c) => {
                self.announce(ctx, c.internalCalls, c.id, c.description, c.uri, privileged)?;
                Bytes::new()
            }

            // --- Batched mint ---
            SC::batchMint(c) => {
                self.batch_mint(caller, c.recipients, c.amounts, privileged)?;
                Bytes::new()
            }

            // --- Security redeem (overrides IB20 redeem semantics) ---
            SC::redeem(c) => {
                self.security_redeem(caller, c.amount)?;
                Bytes::new()
            }
            SC::redeemWithMemo(c) => {
                self.security_redeem_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Minimum redeemable (security version, in scaled units) ---
            SC::minimumRedeemable(_) => self.accounting().minimum_redeemable()?.abi_encode().into(),
            SC::updateMinimumRedeemable(c) => {
                self.update_minimum_redeemable(caller, c.newMinimumRedeemable, privileged)?;
                Bytes::new()
            }

            // --- Security identifier mutations ---
            SC::updateExtraMetadata(c) => {
                self.update_extra_metadata(caller, c.identifierType, c.value, privileged)?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }

    /// Posts an announcement and atomically executes `internal_calls` via self-dispatch.
    ///
    /// The `in_announcement` flag and selector check prevent recursive invocation.
    fn announce(
        &mut self,
        ctx: StorageCtx<'_>,
        internal_calls: Vec<Bytes>,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> base_precompile_storage::Result<()> {
        let caller = ctx.caller();
        self.ensure_operator_role(caller, privileged)?;
        if self.is_announcement_active() {
            return Err(BasePrecompileError::revert(IB20Asset::AnnouncementInProgress {}));
        }

        if self.accounting().is_announcement_id_used(id.as_str())? {
            return Err(BasePrecompileError::revert(IB20Asset::AnnouncementIdAlreadyUsed { id }));
        }
        self.accounting_mut().mark_announcement_id_used(id.as_str())?;

        self.accounting_mut().emit_event(
            IB20Asset::Announcement { caller, id: id.clone(), description, uri }.encode_log_data(),
        )?;

        self.begin_announcement();

        for call in &internal_calls {
            let call_bytes: &[u8] = call.as_ref();
            if call_bytes.len() < 4 {
                return Err(BasePrecompileError::revert(IB20Asset::InternalCallMalformed {
                    call: call.clone(),
                }));
            }
            if call_bytes[..4] == IB20Asset::announceCall::SELECTOR {
                return Err(BasePrecompileError::revert(IB20Asset::AnnouncementInProgress {}));
            }
            self.inner_with_privilege(ctx, call_bytes, privileged).map_err(|_| {
                BasePrecompileError::revert(IB20Asset::InternalCallFailed { call: call.clone() })
            })?;
        }

        self.accounting_mut().emit_event(IB20Asset::EndAnnouncement { id }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_sol_types::{SolCall, SolEvent};
    use base_precompile_storage::{
        BasePrecompileError, HashMapStorageProvider, Result, StorageCtx, setup_storage,
    };

    use crate::{
        ActivationFeature, ActivationRegistryStorage, B20AssetStorage, B20AssetToken,
        B20PausableFeature, B20TokenRole, IB20, IB20Asset, InMemoryPolicy, InMemoryTokenAccounting,
        PolicyHandle, PolicyRegistryStorage, SecurityAccounting, Token, TokenAccounting,
    };

    type TestSecurityToken = B20AssetToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const REDEEM_SENDER_POLICY: B256 = TestSecurityToken::REDEEM_SENDER_POLICY;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);
    const ACTIVATION_ADMIN: Address = Address::repeat_byte(0xcb);
    fn make_token() -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD; // 1:1 multiplier
        // Explicitly open redemption so non-policy tests are not blocked by the ALWAYS_BLOCK default.
        accounting.policy_ids.insert(REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn activate_b20_asset(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationFeature::B20Asset.id(), Some(ACTIVATION_ADMIN))
        })
        .unwrap();
    }

    fn storage_with_caller(caller: Address) -> HashMapStorageProvider {
        let mut storage = HashMapStorageProvider::new(1);
        activate_b20_asset(&mut storage);
        storage.set_caller(caller);
        storage
    }

    fn call_security(
        token: &mut TestSecurityToken,
        caller: Address,
        calldata: Vec<u8>,
    ) -> Result<Bytes> {
        let mut storage = storage_with_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| token.inner(ctx, calldata.as_ref()))
    }

    fn batch_mint_calldata(recipients: Vec<Address>, amounts: Vec<U256>) -> Vec<u8> {
        IB20Asset::batchMintCall { recipients, amounts }.abi_encode()
    }

    #[test]
    fn to_scaled_balance_one_to_one_multiplier() {
        let token = make_token();
        assert_eq!(token.to_scaled_balance(U256::from(100u64)).unwrap(), U256::from(100u64));
    }

    #[test]
    fn to_scaled_balance_two_to_one_multiplier() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD * U256::from(2u64);
        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_scaled_balance(U256::from(50u64)).unwrap(), U256::from(100u64));
    }

    #[test]
    fn batch_mint_increases_balances() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        call_security(
            &mut token,
            ALICE,
            batch_mint_calldata(
                alloc::vec![ALICE, BOB],
                alloc::vec![U256::from(100u64), U256::from(200u64)],
            ),
        )
        .unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(200u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(300u64));
        assert_eq!(
            token.accounting().events,
            alloc::vec![
                IB20::Transfer { from: Address::ZERO, to: ALICE, amount: U256::from(100u64) }
                    .encode_log_data(),
                IB20::Transfer { from: Address::ZERO, to: BOB, amount: U256::from(200u64) }
                    .encode_log_data()
            ]
        );
    }

    #[test]
    fn batch_mint_requires_mint_role() {
        let mut token = make_token();

        let err = call_security(
            &mut token,
            ALICE,
            batch_mint_calldata(alloc::vec![BOB], alloc::vec![U256::from(100u64)]),
        )
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: B20TokenRole::Mint.id(),
            })
        );
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().total_supply().unwrap(), U256::ZERO);
    }

    #[test]
    fn security_redeem_burns_and_emits_security_event() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        token.security_redeem(ALICE, U256::from(50u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().events.len(), 2); // Transfer + Redeemed
    }

    #[test]
    fn security_redeem_zero_amount_is_no_op() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        token.security_redeem(ALICE, U256::ZERO).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 2); // Transfer(ALICE, 0x0, 0) + Redeemed(ALICE, 0, multiplier)
    }

    #[test]
    fn security_redeem_rejects_below_minimum_scaled_amount() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(10u64);

        // 5 tokens * 1e18 multiplier / 1e18 = 5 scaled < 10 minimum
        assert!(token.security_redeem(ALICE, U256::from(5u64)).is_err());
    }

    #[test]
    fn security_redeem_rejects_zero_scaled_amount() {
        let mut token = make_token();
        token.accounting_mut().multiplier = U256::ONE;
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        // 1 token-wei * 1 / WAD rounds down to 0 scaled units, which is always rejected.
        assert!(token.security_redeem(ALICE, U256::ONE).is_err());
    }

    #[test]
    fn security_redeem_rejects_when_redeem_feature_paused() {
        let mut token = make_token();
        token.accounting_mut().paused = B20PausableFeature::mask(IB20::PausableFeature::REDEEM);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        assert_eq!(
            token.security_redeem(ALICE, U256::from(1u64)).unwrap_err(),
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::REDEEM,
            })
        );
    }

    #[test]
    fn security_redeem_rejects_when_sender_policy_denies() {
        let policy_id = 7;
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD;
        accounting.balances.insert(ALICE, U256::from(100u64));
        accounting.total_supply = U256::from(100u64);
        accounting.policy_ids.insert(REDEEM_SENDER_POLICY, policy_id);
        let mut policy = InMemoryPolicy::new();
        policy.create_existing_policy(policy_id);
        let mut token = TestSecurityToken::with_storage_and_policy(accounting, policy);

        assert_eq!(
            token.security_redeem(ALICE, U256::from(1u64)).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: REDEEM_SENDER_POLICY,
                policyId: policy_id,
            })
        );
    }

    #[test]
    fn announce_marks_id_used() {
        let mut token = make_token();
        let id = "2026-Q1-split";

        assert!(!token.accounting().is_announcement_id_used(id).unwrap());
        token.accounting_mut().mark_announcement_id_used(id).unwrap();
        assert!(token.accounting().is_announcement_id_used(id).unwrap());
    }

    #[test]
    fn extra_metadata_roundtrip() {
        let mut token = make_token();

        assert_eq!(token.accounting().extra_metadata("ISIN").unwrap(), "");
        token
            .accounting_mut()
            .set_extra_metadata_value("ISIN", "US0000000000".to_string())
            .unwrap();
        assert_eq!(token.accounting().extra_metadata("ISIN").unwrap(), "US0000000000".to_string());
    }

    // --- batchMint: EmptyBatch / LengthMismatch ---

    #[test]
    fn batch_mint_rejects_empty() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        assert_eq!(
            call_security(&mut token, ALICE, batch_mint_calldata(alloc::vec![], alloc::vec![]))
                .unwrap_err(),
            BasePrecompileError::revert(IB20Asset::EmptyBatch {})
        );
    }

    #[test]
    fn batch_mint_rejects_length_mismatch() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        assert_eq!(
            call_security(
                &mut token,
                ALICE,
                batch_mint_calldata(alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20Asset::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );

        assert_eq!(
            call_security(
                &mut token,
                ALICE,
                batch_mint_calldata(alloc::vec![], alloc::vec![U256::ONE]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20Asset::LengthMismatch {
                leftLen: U256::ZERO,
                rightLen: U256::ONE,
            })
        );
    }

    // --- redeem: InsufficientBalance / boundary / scaled math / event pair ---

    #[test]
    fn security_redeem_rejects_insufficient_balance() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        token.accounting_mut().total_supply = U256::from(10u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);
        // amount=100 > balance=10 → InsufficientBalance after the scaled-floor check passes
        assert!(token.security_redeem(ALICE, U256::from(100u64)).is_err());
        // no state mutation on failure
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(10u64));
    }

    #[test]
    fn security_redeem_supply_underflow_is_under_overflow_panic() {
        // Regression: before BOP-160, saturating_sub silently clamped supply to zero.
        // If an accounting bug left total_supply < balance, the correct behavior is to
        // revert with an arithmetic under/overflow panic rather than zeroing the supply.
        let mut token = make_token();
        // Supply invariant violated: balance exceeds total supply.
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        // amount=75 passes the balance check (100 >= 75) but underflows total_supply (50 - 75).
        assert_eq!(
            token.security_redeem(ALICE, U256::from(75u64)).unwrap_err(),
            BasePrecompileError::under_overflow()
        );
    }

    #[test]
    fn security_redeem_at_exact_minimum_succeeds() {
        let mut token = make_token(); // 1:1 multiplier
        token.accounting_mut().balances.insert(ALICE, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);
        // 5 tokens * WAD / WAD = 5 scaled == minimum → boundary must be accepted
        token.accounting_mut().minimum_redeemable = U256::from(5u64);
        token.security_redeem(ALICE, U256::from(5u64)).unwrap();
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(45u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(45u64));
    }

    #[test]
    fn security_redeem_with_non_unit_multiplier_applies_correct_scaled_math() {
        let mut token = make_token();
        // 2x multiplier: 1 token scales to 2 units
        token.accounting_mut().multiplier = B20AssetStorage::WAD * U256::from(2u64);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        // minimum = 10 scaled → need at least 5 tokens
        token.accounting_mut().minimum_redeemable = U256::from(10u64);
        // 4 tokens → 8 scaled < 10 → BelowMinimumRedeemable
        assert!(token.security_redeem(ALICE, U256::from(4u64)).is_err());
        // 5 tokens → 10 scaled == minimum → accepted
        token.security_redeem(ALICE, U256::from(5u64)).unwrap();
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(95u64));
    }

    #[test]
    fn security_redeem_emits_transfer_then_redeemed() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);
        token.security_redeem(ALICE, U256::from(10u64)).unwrap();
        // "Emits Transfer(caller, address(0), amount) followed by Redeemed(caller, amount, multiplier)"
        assert_eq!(token.accounting().events.len(), 2);
    }

    #[test]
    fn security_redeem_with_memo_emits_memo_before_redeemed() {
        let mut token = make_token();
        let amount = U256::from(10u64);
        let memo = B256::repeat_byte(0x42);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        token.security_redeem_with_memo(ALICE, amount, memo).unwrap();

        assert_eq!(
            token.accounting().events[0],
            IB20::Transfer { from: ALICE, to: Address::ZERO, amount }.encode_log_data()
        );
        assert_eq!(
            token.accounting().events[1],
            IB20::Memo { caller: ALICE, memo }.encode_log_data()
        );
        assert_eq!(
            token.accounting().events[2],
            IB20Asset::Redeemed { from: ALICE, amt: amount, multiplier: B20AssetStorage::WAD }
                .encode_log_data()
        );
    }

    // --- toScaledBalance: zero balance / sub-WAD truncation / scaledBalanceOf delegation ---

    #[test]
    fn to_scaled_balance_zero_balance_yields_zero() {
        let token = make_token();
        assert_eq!(token.to_scaled_balance(U256::ZERO).unwrap(), U256::ZERO);
    }

    #[test]
    fn to_scaled_balance_sub_wad_multiplier_truncates_to_zero() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        // 0.5 WAD: 1 token → 0.5 scaled → truncates to 0 via integer division
        accounting.multiplier = B20AssetStorage::WAD / U256::from(2u64);
        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_scaled_balance(U256::from(1u64)).unwrap(), U256::ZERO);
    }

    #[test]
    fn scaled_balance_of_derives_from_balance() {
        let mut token = make_token(); // 1:1 multiplier
        token.accounting_mut().balances.insert(ALICE, U256::from(75u64));
        // scaledBalanceOf(account) = toScaledBalance(balanceOf(account))
        let balance = token.accounting().balance_of(ALICE).unwrap();
        assert_eq!(token.to_scaled_balance(balance).unwrap(), U256::from(75u64));
    }

    #[test]
    fn storage_backed_redeem_uses_wad_when_multiplier_slot_is_unset() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetToken::with_storage_and_policy(
                B20AssetStorage::from_address(TOKEN, ctx),
                PolicyHandle::new(ctx),
            );
            token.accounting_mut().set_balance(ALICE, U256::from(100u64)).unwrap();
            token.accounting_mut().set_total_supply(U256::from(100u64)).unwrap();
            token.accounting_mut().set_minimum_redeemable(U256::from(10u64)).unwrap();

            assert_eq!(token.accounting().multiplier().unwrap(), B20AssetStorage::WAD);
            token.security_redeem(ALICE, U256::from(10u64)).unwrap();

            assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(90u64));
            assert_eq!(token.accounting().total_supply().unwrap(), U256::from(90u64));
        });
    }

    // --- updateMultiplier: persistence ---

    #[test]
    fn multiplier_update_persists() {
        let mut token = make_token();
        let new_multiplier = B20AssetStorage::WAD * U256::from(3u64);
        token.accounting_mut().set_multiplier(new_multiplier).unwrap();
        assert_eq!(token.accounting().multiplier().unwrap(), new_multiplier);
    }

    // --- extraMetadata / updateExtraMetadata ---

    #[test]
    fn extra_metadata_missing_key_returns_empty() {
        let token = make_token();
        // "Returns the empty string if not set"
        assert_eq!(token.accounting().extra_metadata("CUSIP").unwrap(), "");
    }

    #[test]
    fn extra_metadata_empty_value_clears_entry() {
        let mut token = make_token();
        token
            .accounting_mut()
            .set_extra_metadata_value("FIGI", "BBG000B9XRY4".to_string())
            .unwrap();
        assert_eq!(token.accounting().extra_metadata("FIGI").unwrap(), "BBG000B9XRY4");
        // "passing an empty value removes the entry"
        token.accounting_mut().set_extra_metadata_value("FIGI", String::new()).unwrap();
        assert_eq!(token.accounting().extra_metadata("FIGI").unwrap(), "");
    }

    // --- minimumRedeemable / updateMinimumRedeemable ---

    #[test]
    fn minimum_redeemable_persists() {
        let mut token = make_token();
        let floor = U256::from(42u64);
        token.accounting_mut().set_minimum_redeemable(floor).unwrap();
        assert_eq!(token.accounting().minimum_redeemable().unwrap(), floor);
    }

    // --- isAnnouncementIdUsed: fresh state ---

    #[test]
    fn announcement_id_not_used_initially() {
        let token = make_token();
        let id = "2026-Q1-split";
        // "Returns true if id has previously been consumed by announce" → false for new id
        assert!(!token.accounting().is_announcement_id_used(id).unwrap());
    }

    /// `to_scaled_balance` must return an arithmetic overflow panic rather than silently
    /// saturating when `rawBalance * multiplier` exceeds `U256::MAX`.
    #[test]
    fn to_scaled_balance_overflows_when_product_exceeds_u256_max() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        // Any balance > 1 overflows when multiplied by this multiplier.
        accounting.multiplier = U256::MAX / U256::from(2u64) + U256::ONE;
        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.to_scaled_balance(U256::from(2u64)).unwrap_err(),
            BasePrecompileError::under_overflow()
        );
    }

    /// `security_redeem` must return an arithmetic overflow panic rather than
    /// silently saturating when `amount * multiplier` exceeds `U256::MAX`.
    #[test]
    fn security_redeem_overflows_when_product_exceeds_u256_max() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        // Any amount > 1 overflows when multiplied by this multiplier.
        accounting.multiplier = U256::MAX / U256::from(2u64) + U256::ONE;
        accounting.balances.insert(ALICE, U256::from(2u64));
        accounting.total_supply = U256::from(2u64);
        // Open redemption so the policy gate does not block the call before the overflow.
        accounting.policy_ids.insert(REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        let mut token =
            TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.security_redeem(ALICE, U256::from(2u64)).unwrap_err(),
            BasePrecompileError::under_overflow()
        );
    }
}
