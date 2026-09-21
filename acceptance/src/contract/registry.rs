//! Activation-registry, policy-registry, and policy-gated transfer workloads.

use std::time::Duration;

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, LogData, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_network::Base;
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20PolicyType, B20Variant, IActivationRegistry,
    IB20, IPolicyRegistry, PolicyRegistryStorage,
};
use base_common_rpc_types::BaseTransactionReceipt;
use eyre::{Result, WrapErr, ensure};
use serde_json::{Value, json};

use crate::{B20PrecompileClient, ContractCase, WorkloadContext};

/// Complete RPC acceptance workloads for the activation and policy registries.
#[derive(Debug)]
pub struct RegistryWorkload;

impl RegistryWorkload {
    /// Executes one registry or policy-transfer contract case.
    pub async fn execute(case: ContractCase, context: &WorkloadContext<'_>) -> Result<Value> {
        let provider = context.provider("builder")?;
        let admin = WorkloadContext::signer(5)?;
        let other = WorkloadContext::signer(6)?;
        let third = WorkloadContext::signer(7)?;
        let chain_id = context.config.devnet.l2.chain_id;
        let client = B20PrecompileClient::new(&provider, &admin, chain_id);

        match case {
            ContractCase::ActivationRegistryIsActivatedDefault => {
                let active = Self::is_activated(&client, ActivationFeature::B20Asset.id()).await?;
                ensure!(!active, "feature should be inactive by default");
                Ok(json!({ "feature": ActivationFeature::B20Asset.id(), "active": active }))
            }
            ContractCase::ActivationRegistryAdmin => {
                let actual = Self::activation_admin(&client).await?;
                ensure!(actual == admin.address(), "activation admin mismatch");
                Ok(json!({ "expected_admin": admin.address(), "observed_admin": actual }))
            }
            ContractCase::ActivationRegistrySetAdminRevertsBeforeCobalt => {
                let receipt = client
                    .send_call_unchecked_receipt(
                        ActivationRegistryStorage::ADDRESS,
                        IActivationRegistry::setAdminCall { newAdmin: other.address() },
                        "set activation admin before Cobalt",
                    )
                    .await?;
                ensure!(!receipt.status(), "setAdmin should revert before Cobalt");
                let observed = Self::activation_admin(&client).await?;
                ensure!(observed == admin.address(), "failed setAdmin changed admin");
                Ok(
                    json!({ "transaction_hash": receipt.transaction_hash(), "status": receipt.status(), "observed_admin": observed }),
                )
            }
            ContractCase::ActivationRegistryCobaltAdminRotation => {
                Self::activation_admin_rotation(&provider, &client, &admin, &other, chain_id).await
            }
            ContractCase::ActivationRegistryAdminLifecycle => {
                Self::activation_lifecycle(&client, admin.address()).await
            }
            ContractCase::ActivationRegistryUnauthorizedActivateReverts => {
                let unauthorized = B20PrecompileClient::new(&provider, &other, chain_id);
                let receipt = unauthorized
                    .send_call_unchecked_receipt(
                        ActivationRegistryStorage::ADDRESS,
                        IActivationRegistry::activateCall {
                            feature: ActivationFeature::B20Asset.id(),
                        },
                        "unauthorized activation",
                    )
                    .await?;
                ensure!(!receipt.status(), "unauthorized activation succeeded");
                let active = Self::is_activated(&client, ActivationFeature::B20Asset.id()).await?;
                ensure!(!active, "unauthorized activation changed state");
                Ok(
                    json!({ "transaction_hash": receipt.transaction_hash(), "status": receipt.status(), "active_after": active }),
                )
            }
            ContractCase::ActivationRegistryCheckActivatedGate => {
                Self::activation_check_gate(&client).await
            }
            ContractCase::PolicyRegistryCreatePolicyEmitsEvents => {
                Self::policy_creation_events(&client).await
            }
            ContractCase::PolicyRegistryPolicyExists => Self::policy_existence(&client).await,
            ContractCase::PolicyRegistryLifecycleAndErrorPaths => {
                Self::policy_lifecycle(&provider, &client, &admin, &other, &third, chain_id).await
            }
            ContractCase::PolicyRegistryDeactivatedViewsAndWriteGate => {
                Self::policy_deactivated(&client, &admin, &other).await
            }
            ContractCase::AllowlistGatesTransfer => {
                Self::allowlist_transfer(&provider, &client, &admin, &other, &third, chain_id).await
            }
            ContractCase::BlocklistGatesTransfer => {
                Self::blocklist_transfer(&provider, &client, &admin, &other, &third, chain_id).await
            }
            ContractCase::AlwaysBlockPolicyBlocksTransfer => {
                Self::always_block_transfer(&client, &admin, &other).await
            }
            _ => eyre::bail!("RegistryWorkload cannot execute {case:?}"),
        }
    }

    /// Reads a feature's activation state.
    pub async fn is_activated(client: &B20PrecompileClient<'_>, feature: B256) -> Result<bool> {
        let output = client
            .call(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::isActivatedCall { feature },
            )
            .await?;
        IActivationRegistry::isActivatedCall::abi_decode_returns(&output)
            .wrap_err("decode isActivated")
    }

    /// Reads the activation registry admin.
    pub async fn activation_admin(client: &B20PrecompileClient<'_>) -> Result<Address> {
        let output = client
            .call(ActivationRegistryStorage::ADDRESS, IActivationRegistry::adminCall {})
            .await?;
        IActivationRegistry::adminCall::abi_decode_returns(&output).wrap_err("decode admin")
    }

    /// Verifies one exact receipt log, including emitter, topics, and data.
    pub fn require_log(
        receipt: &BaseTransactionReceipt,
        emitter: Address,
        expected: &LogData,
    ) -> Result<Value> {
        let found = receipt
            .inner
            .logs()
            .iter()
            .find(|log| log.address() == emitter && log.data() == expected);
        ensure!(
            found.is_some(),
            "receipt missing exact log: emitter={emitter}, expected={expected:?}, logs={:?}",
            receipt.inner.logs()
        );
        Ok(json!({
            "emitter": emitter,
            "topics": expected.topics().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "data": expected.data.to_string(),
        }))
    }

    /// Runs activate, repeated-activate, deactivate, repeated-deactivate, and reactivate.
    pub async fn activation_lifecycle(
        client: &B20PrecompileClient<'_>,
        admin: Address,
    ) -> Result<Value> {
        let feature = ActivationFeature::B20Stablecoin.id();
        ensure!(!Self::is_activated(client, feature).await?, "feature initially active");
        let activated = client
            .send_call_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall { feature },
                "activate feature",
            )
            .await?;
        let activate_log = Self::require_log(
            &activated,
            ActivationRegistryStorage::ADDRESS,
            &IActivationRegistry::FeatureActivated { feature, caller: admin }.encode_log_data(),
        )?;
        ensure!(Self::is_activated(client, feature).await?, "activation not persisted");
        let repeat_activate = client
            .send_call_unchecked_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall { feature },
                "repeat activation",
            )
            .await?;
        ensure!(!repeat_activate.status(), "repeated activation succeeded");
        ensure!(Self::is_activated(client, feature).await?, "repeat activation changed state");
        let deactivated = client
            .send_call_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::deactivateCall { feature },
                "deactivate feature",
            )
            .await?;
        let deactivate_log = Self::require_log(
            &deactivated,
            ActivationRegistryStorage::ADDRESS,
            &IActivationRegistry::FeatureDeactivated { feature, caller: admin }.encode_log_data(),
        )?;
        ensure!(!Self::is_activated(client, feature).await?, "deactivation not persisted");
        let repeat_deactivate = client
            .send_call_unchecked_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::deactivateCall { feature },
                "repeat deactivation",
            )
            .await?;
        ensure!(!repeat_deactivate.status(), "repeated deactivation succeeded");
        ensure!(!Self::is_activated(client, feature).await?, "repeat deactivation changed state");
        let reactivated = client
            .send_call_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall { feature },
                "reactivate feature",
            )
            .await?;
        let reactivate_log = Self::require_log(
            &reactivated,
            ActivationRegistryStorage::ADDRESS,
            &IActivationRegistry::FeatureActivated { feature, caller: admin }.encode_log_data(),
        )?;
        ensure!(Self::is_activated(client, feature).await?, "reactivation not persisted");
        Ok(
            json!({ "feature": feature, "activate_log": activate_log, "deactivate_log": deactivate_log, "reactivate_log": reactivate_log, "repeat_activate_receipt": repeat_activate.transaction_hash(), "repeat_deactivate_receipt": repeat_deactivate.transaction_hash(), "final_active": true }),
        )
    }

    /// Verifies Cobalt admin rotation, its event, and authority transfer.
    pub async fn activation_admin_rotation(
        provider: &RootProvider<Base>,
        client: &B20PrecompileClient<'_>,
        admin: &PrivateKeySigner,
        next: &PrivateKeySigner,
        chain_id: u64,
    ) -> Result<Value> {
        ensure!(
            Self::activation_admin(client).await? == admin.address(),
            "unexpected initial admin"
        );
        let receipt = client
            .send_call_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::setAdminCall { newAdmin: next.address() },
                "set activation admin",
            )
            .await?;
        let log = Self::require_log(
            &receipt,
            ActivationRegistryStorage::ADDRESS,
            &IActivationRegistry::AdminChanged {
                previousAdmin: admin.address(),
                newAdmin: next.address(),
                caller: admin.address(),
            }
            .encode_log_data(),
        )?;
        ensure!(Self::activation_admin(client).await? == next.address(), "new admin not stored");
        let feature = ActivationFeature::B20Stablecoin.id();
        let old_receipt = client
            .send_call_unchecked_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall { feature },
                "old admin activation",
            )
            .await?;
        ensure!(!old_receipt.status(), "old admin retained activation authority");
        let next_client = B20PrecompileClient::new(provider, next, chain_id);
        let activation = next_client
            .send_call_receipt(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall { feature },
                "new admin activation",
            )
            .await?;
        let activation_log = Self::require_log(
            &activation,
            ActivationRegistryStorage::ADDRESS,
            &IActivationRegistry::FeatureActivated { feature, caller: next.address() }
                .encode_log_data(),
        )?;
        ensure!(Self::is_activated(&next_client, feature).await?, "new admin activation absent");
        Ok(
            json!({ "admin_change_log": log, "activation_log": activation_log, "old_admin_receipt": old_receipt.transaction_hash(), "observed_admin": next.address(), "active": true }),
        )
    }

    /// Verifies the checkActivated read gate before, during, and after activation.
    pub async fn activation_check_gate(client: &B20PrecompileClient<'_>) -> Result<Value> {
        let feature = ActivationFeature::B20Asset.id();
        let before = client
            .call(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::checkActivatedCall { feature },
            )
            .await;
        let inactive_error = before
            .err()
            .ok_or_else(|| eyre::eyre!("inactive checkActivated unexpectedly succeeded"))?;
        ensure!(
            Self::is_execution_revert(&inactive_error),
            "inactive checkActivated failed without an RPC execution revert: {inactive_error}"
        );
        client.activate_feature(feature).await?;
        ensure!(Self::is_activated(client, feature).await?, "feature did not activate");
        client
            .call(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::checkActivatedCall { feature },
            )
            .await
            .wrap_err("active checkActivated failed")?;
        client.deactivate_feature(feature).await?;
        ensure!(!Self::is_activated(client, feature).await?, "feature did not deactivate");
        Ok(
            json!({ "feature": feature, "inactive_error": inactive_error.to_string(), "active_call_succeeded": true, "final_active": false }),
        )
    }

    /// Returns whether an error chain contains a structured RPC execution error response.
    pub fn is_execution_revert(error: &eyre::Report) -> bool {
        error.chain().any(|source| {
            source
                .downcast_ref::<alloy_transport::TransportError>()
                .is_some_and(|error| matches!(error, alloy_transport::RpcError::ErrorResp(payload) if payload.code == 3))
        })
    }

    /// Creates a policy and returns its simulated deterministic ID after submitting it.
    pub async fn create_policy(
        client: &B20PrecompileClient<'_>,
        admin: Address,
        policy_type: IPolicyRegistry::PolicyType,
        label: &'static str,
    ) -> Result<u64> {
        let call = IPolicyRegistry::createPolicyCall { admin, policyType: policy_type };
        let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
        let id = IPolicyRegistry::createPolicyCall::abi_decode_returns(&output)
            .wrap_err("decode createPolicy")?;
        client.send_call(PolicyRegistryStorage::ADDRESS, call, label).await?;
        Ok(id)
    }

    /// Creates a policy initialized with membership accounts.
    pub async fn create_policy_with_accounts(
        client: &B20PrecompileClient<'_>,
        admin: Address,
        policy_type: IPolicyRegistry::PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        let call = IPolicyRegistry::createPolicyWithAccountsCall {
            admin,
            policyType: policy_type,
            accounts,
        };
        let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
        let id = IPolicyRegistry::createPolicyWithAccountsCall::abi_decode_returns(&output)
            .wrap_err("decode createPolicyWithAccounts")?;
        client
            .send_call(PolicyRegistryStorage::ADDRESS, call, "create policy with accounts")
            .await?;
        Ok(id)
    }

    /// Reads whether a policy exists.
    pub async fn policy_exists(client: &B20PrecompileClient<'_>, id: u64) -> Result<bool> {
        let output = client
            .call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::policyExistsCall { policyId: id },
            )
            .await?;
        IPolicyRegistry::policyExistsCall::abi_decode_returns(&output)
            .wrap_err("decode policyExists")
    }

    /// Reads a policy's current admin.
    pub async fn policy_admin(client: &B20PrecompileClient<'_>, id: u64) -> Result<Address> {
        let output = client
            .call(PolicyRegistryStorage::ADDRESS, IPolicyRegistry::policyAdminCall { policyId: id })
            .await?;
        IPolicyRegistry::policyAdminCall::abi_decode_returns(&output).wrap_err("decode policyAdmin")
    }

    /// Reads a policy's pending admin.
    pub async fn pending_policy_admin(
        client: &B20PrecompileClient<'_>,
        id: u64,
    ) -> Result<Address> {
        let output = client
            .call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::pendingPolicyAdminCall { policyId: id },
            )
            .await?;
        IPolicyRegistry::pendingPolicyAdminCall::abi_decode_returns(&output)
            .wrap_err("decode pendingPolicyAdmin")
    }

    /// Reads policy authorization for one account.
    pub async fn is_authorized(
        client: &B20PrecompileClient<'_>,
        id: u64,
        account: Address,
    ) -> Result<bool> {
        let output = client
            .call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::isAuthorizedCall { policyId: id, account },
            )
            .await?;
        IPolicyRegistry::isAuthorizedCall::abi_decode_returns(&output)
            .wrap_err("decode isAuthorized")
    }

    /// Verifies exact `PolicyCreated` and `PolicyAdminUpdated` receipt logs.
    pub async fn policy_creation_events(client: &B20PrecompileClient<'_>) -> Result<Value> {
        client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;
        let admin = client.signer_address();
        let call = IPolicyRegistry::createPolicyCall {
            admin,
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        };
        let output = client.call(PolicyRegistryStorage::ADDRESS, call.clone()).await?;
        let id = IPolicyRegistry::createPolicyCall::abi_decode_returns(&output)?;
        let receipt = client
            .send_call_receipt(PolicyRegistryStorage::ADDRESS, call, "create allowlist policy")
            .await?;
        let created = Self::require_log(
            &receipt,
            PolicyRegistryStorage::ADDRESS,
            &IPolicyRegistry::PolicyCreated {
                policyId: id,
                creator: admin,
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            }
            .encode_log_data(),
        )?;
        let admin_updated = Self::require_log(
            &receipt,
            PolicyRegistryStorage::ADDRESS,
            &IPolicyRegistry::PolicyAdminUpdated {
                policyId: id,
                previousAdmin: Address::ZERO,
                newAdmin: admin,
            }
            .encode_log_data(),
        )?;
        Ok(
            json!({ "policy_id": id, "transaction_hash": receipt.transaction_hash(), "policy_created_log": created, "admin_updated_log": admin_updated }),
        )
    }

    /// Verifies built-in policy existence and a nonexistent dynamic ID.
    pub async fn policy_existence(client: &B20PrecompileClient<'_>) -> Result<Value> {
        client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;
        let allow = Self::policy_exists(client, PolicyRegistryStorage::ALWAYS_ALLOW_ID).await?;
        let block = Self::policy_exists(client, PolicyRegistryStorage::ALWAYS_BLOCK_ID).await?;
        let missing_id = u64::MAX;
        let missing = Self::policy_exists(client, missing_id).await?;
        ensure!(allow, "ALWAYS_ALLOW policy missing");
        ensure!(block, "ALWAYS_BLOCK policy missing");
        ensure!(!missing, "nonexistent policy unexpectedly exists");
        Ok(
            json!({ "always_allow": { "id": PolicyRegistryStorage::ALWAYS_ALLOW_ID, "exists": allow }, "always_block": { "id": PolicyRegistryStorage::ALWAYS_BLOCK_ID, "exists": block }, "nonexistent": { "id": missing_id, "exists": missing } }),
        )
    }

    /// Runs policy membership, type errors, admin handoff, renounce, and error paths.
    pub async fn policy_lifecycle(
        provider: &RootProvider<Base>,
        client: &B20PrecompileClient<'_>,
        admin: &PrivateKeySigner,
        next: &PrivateKeySigner,
        member: &PrivateKeySigner,
        chain_id: u64,
    ) -> Result<Value> {
        client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;
        let allow_id = Self::create_policy(
            client,
            admin.address(),
            IPolicyRegistry::PolicyType::ALLOWLIST,
            "create allowlist policy",
        )
        .await?;
        ensure!(Self::policy_exists(client, allow_id).await?, "dynamic policy absent");
        ensure!(Self::policy_admin(client, allow_id).await? == admin.address(), "admin mismatch");
        ensure!(
            Self::pending_policy_admin(client, allow_id).await? == Address::ZERO,
            "pending admin should be zero"
        );
        ensure!(
            !Self::is_authorized(client, allow_id, member.address()).await?,
            "fresh allowlist authorized member"
        );
        client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: allow_id,
                    allowed: true,
                    accounts: vec![member.address()],
                },
                "add allowlist member",
            )
            .await?;
        ensure!(
            Self::is_authorized(client, allow_id, member.address()).await?,
            "allowlist update absent"
        );
        let block_id = Self::create_policy_with_accounts(
            client,
            admin.address(),
            IPolicyRegistry::PolicyType::BLOCKLIST,
            vec![member.address()],
        )
        .await?;
        ensure!(
            !Self::is_authorized(client, block_id, member.address()).await?,
            "blocklist member authorized"
        );
        let wrong_allow = client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: block_id,
                    allowed: true,
                    accounts: vec![member.address()],
                },
                "allowlist update on blocklist",
            )
            .await?;
        ensure!(!wrong_allow.status(), "allowlist update on blocklist succeeded");
        ensure!(
            !Self::is_authorized(client, block_id, member.address()).await?,
            "failed wrong-type update changed blocklist"
        );
        let wrong_block = client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateBlocklistCall {
                    policyId: allow_id,
                    blocked: true,
                    accounts: vec![member.address()],
                },
                "blocklist update on allowlist",
            )
            .await?;
        ensure!(!wrong_block.status(), "blocklist update on allowlist succeeded");
        ensure!(
            Self::is_authorized(client, allow_id, member.address()).await?,
            "failed wrong-type update changed allowlist"
        );
        client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::stageUpdateAdminCall {
                    policyId: allow_id,
                    newAdmin: next.address(),
                },
                "stage policy admin",
            )
            .await?;
        ensure!(
            Self::pending_policy_admin(client, allow_id).await? == next.address(),
            "pending admin mismatch"
        );
        let next_client = B20PrecompileClient::new(provider, next, chain_id);
        next_client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::finalizeUpdateAdminCall { policyId: allow_id },
                "finalize policy admin",
            )
            .await?;
        ensure!(
            Self::policy_admin(client, allow_id).await? == next.address(),
            "admin handoff absent"
        );
        ensure!(
            Self::pending_policy_admin(client, allow_id).await? == Address::ZERO,
            "pending admin not cleared"
        );
        let old_admin = client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: allow_id,
                    allowed: false,
                    accounts: vec![member.address()],
                },
                "old admin policy update",
            )
            .await?;
        ensure!(!old_admin.status(), "old admin retained authority");
        ensure!(
            Self::is_authorized(client, allow_id, member.address()).await?,
            "old-admin revert changed membership"
        );
        next_client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::renounceAdminCall { policyId: allow_id },
                "renounce policy admin",
            )
            .await?;
        ensure!(
            Self::policy_admin(client, allow_id).await? == Address::ZERO,
            "renounce did not clear admin"
        );
        let frozen = next_client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: allow_id,
                    allowed: false,
                    accounts: vec![member.address()],
                },
                "renounced policy update",
            )
            .await?;
        ensure!(!frozen.status(), "renounced policy remained mutable");
        ensure!(
            Self::is_authorized(client, allow_id, member.address()).await?,
            "renounced update changed membership"
        );
        let zero_admin = client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::createPolicyCall {
                    admin: Address::ZERO,
                    policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
                },
                "zero-admin policy creation",
            )
            .await?;
        ensure!(!zero_admin.status(), "zero-admin policy creation succeeded");
        let no_pending = client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::finalizeUpdateAdminCall { policyId: allow_id },
                "finalize without pending admin",
            )
            .await?;
        ensure!(!no_pending.status(), "finalize without pending admin succeeded");
        Ok(
            json!({ "allowlist_id": allow_id, "blocklist_id": block_id, "member": member.address(), "member_authorized_after_errors": Self::is_authorized(client, allow_id, member.address()).await?, "final_admin": Self::policy_admin(client, allow_id).await?, "revert_transactions": [wrong_allow.transaction_hash(), wrong_block.transaction_hash(), old_admin.transaction_hash(), frozen.transaction_hash(), zero_admin.transaction_hash(), no_pending.transaction_hash()] }),
        )
    }

    /// Verifies views and preserved state while deactivated, plus the write gate.
    pub async fn policy_deactivated(
        client: &B20PrecompileClient<'_>,
        admin: &PrivateKeySigner,
        blocked: &PrivateKeySigner,
    ) -> Result<Value> {
        client.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;
        let id = Self::create_policy(
            client,
            admin.address(),
            IPolicyRegistry::PolicyType::BLOCKLIST,
            "create blocklist policy",
        )
        .await?;
        client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateBlocklistCall {
                    policyId: id,
                    blocked: true,
                    accounts: vec![blocked.address()],
                },
                "block account",
            )
            .await?;
        client.deactivate_feature(ActivationFeature::PolicyRegistry.id()).await?;
        let exists = Self::policy_exists(client, id).await?;
        let observed_admin = Self::policy_admin(client, id).await?;
        let authorized = Self::is_authorized(client, id, blocked.address()).await?;
        ensure!(exists, "policyExists view failed while deactivated");
        ensure!(observed_admin == admin.address(), "policyAdmin view changed while deactivated");
        ensure!(!authorized, "blocked membership lost while deactivated");
        let write = client
            .send_call_unchecked_receipt(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::createPolicyCall {
                    admin: admin.address(),
                    policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
                },
                "create policy while deactivated",
            )
            .await?;
        ensure!(!write.status(), "policy write succeeded while deactivated");
        ensure!(Self::policy_exists(client, id).await?, "failed write changed existing policy");
        Ok(
            json!({ "policy_id": id, "exists": exists, "admin": observed_admin, "blocked_account": blocked.address(), "authorized": authorized, "write_receipt": write.transaction_hash(), "write_status": write.status() }),
        )
    }

    /// Activates the B20 and policy-registry features.
    pub async fn activate_policy_transfers(client: &B20PrecompileClient<'_>) -> Result<()> {
        client.activate_feature(ActivationFeature::B20Asset.id()).await?;
        client.activate_feature(ActivationFeature::PolicyRegistry.id()).await
    }

    /// Creates an asset token used by transfer-policy acceptance cases.
    pub async fn create_policy_token(
        client: &B20PrecompileClient<'_>,
        admin: Address,
        salt: B256,
        name: &str,
        symbol: &str,
    ) -> Result<Address> {
        let token = client
            .create_token(
                B20Variant::Asset,
                B20PrecompileClient::token_params(
                    name,
                    symbol,
                    admin,
                    U256::from(1_000_000u64),
                    admin,
                ),
                salt,
            )
            .await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        Ok(token)
    }

    /// Wires a policy to a token's transfer-sender scope.
    pub async fn set_transfer_policy(
        client: &B20PrecompileClient<'_>,
        token: Address,
        id: u64,
    ) -> Result<()> {
        client
            .send_call(
                token,
                IB20::updatePolicyCall {
                    policyScope: B20PolicyType::TransferSender.id(),
                    newPolicyId: id,
                },
                "set transfer-sender policy",
            )
            .await
    }

    /// Runs the complete rejected-to-allowed allowlist transfer cycle.
    pub async fn allowlist_transfer(
        provider: &RootProvider<Base>,
        client: &B20PrecompileClient<'_>,
        admin: &PrivateKeySigner,
        recipient: &PrivateKeySigner,
        sender: &PrivateKeySigner,
        chain_id: u64,
    ) -> Result<Value> {
        Self::activate_policy_transfers(client).await?;
        let id = Self::create_policy(
            client,
            admin.address(),
            IPolicyRegistry::PolicyType::ALLOWLIST,
            "create transfer allowlist",
        )
        .await?;
        ensure!(
            !Self::is_authorized(client, id, sender.address()).await?,
            "fresh allowlist authorized sender"
        );
        client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: id,
                    allowed: true,
                    accounts: vec![admin.address()],
                },
                "allowlist seeding admin",
            )
            .await?;
        ensure!(Self::is_authorized(client, id, admin.address()).await?, "admin not allowlisted");
        let token = Self::create_policy_token(
            client,
            admin.address(),
            B256::repeat_byte(0x50),
            "Allowlist Token",
            "ALT",
        )
        .await?;
        Self::set_transfer_policy(client, token, id).await?;
        client.transfer(token, sender.address(), U256::from(100_000u64)).await?;
        ensure!(
            client.balance_of(token, sender.address()).await? == U256::from(100_000u64),
            "seed balance mismatch"
        );
        let sender_client = B20PrecompileClient::new(provider, sender, chain_id);
        let rejected = sender_client
            .send_call_unchecked_receipt(
                token,
                IB20::transferCall { to: recipient.address(), amount: U256::from(50_000u64) },
                "non-member transfer",
            )
            .await?;
        ensure!(!rejected.status(), "non-member transfer succeeded");
        ensure!(
            client.balance_of(token, sender.address()).await? == U256::from(100_000u64),
            "rejected transfer changed sender balance"
        );
        client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: id,
                    allowed: true,
                    accounts: vec![sender.address()],
                },
                "allowlist sender",
            )
            .await?;
        ensure!(
            Self::is_authorized(client, id, sender.address()).await?,
            "sender not authorized after update"
        );
        let accepted = sender_client
            .send_call_receipt(
                token,
                IB20::transferCall { to: recipient.address(), amount: U256::from(50_000u64) },
                "allowlisted transfer",
            )
            .await?;
        let sender_balance = client.balance_of(token, sender.address()).await?;
        let recipient_balance = client.balance_of(token, recipient.address()).await?;
        ensure!(
            sender_balance == U256::from(50_000u64),
            "allowed transfer sender balance mismatch"
        );
        ensure!(
            recipient_balance == U256::from(50_000u64),
            "allowed transfer recipient balance mismatch"
        );
        Ok(
            json!({ "token": token, "policy_id": id, "sender": sender.address(), "rejected_transaction": rejected.transaction_hash(), "accepted_transaction": accepted.transaction_hash(), "sender_balance": sender_balance, "recipient_balance": recipient_balance }),
        )
    }

    /// Runs the complete allowed-to-rejected blocklist transfer cycle.
    pub async fn blocklist_transfer(
        provider: &RootProvider<Base>,
        client: &B20PrecompileClient<'_>,
        admin: &PrivateKeySigner,
        recipient: &PrivateKeySigner,
        sender: &PrivateKeySigner,
        chain_id: u64,
    ) -> Result<Value> {
        Self::activate_policy_transfers(client).await?;
        let id = Self::create_policy(
            client,
            admin.address(),
            IPolicyRegistry::PolicyType::BLOCKLIST,
            "create transfer blocklist",
        )
        .await?;
        ensure!(
            Self::is_authorized(client, id, sender.address()).await?,
            "fresh blocklist rejected sender"
        );
        let token = Self::create_policy_token(
            client,
            admin.address(),
            B256::repeat_byte(0x51),
            "Blocklist Token",
            "BLT",
        )
        .await?;
        Self::set_transfer_policy(client, token, id).await?;
        client.transfer(token, sender.address(), U256::from(100_000u64)).await?;
        let sender_client = B20PrecompileClient::new(provider, sender, chain_id);
        let accepted = sender_client
            .send_call_receipt(
                token,
                IB20::transferCall { to: recipient.address(), amount: U256::from(50_000u64) },
                "unblocked transfer",
            )
            .await?;
        ensure!(
            client.balance_of(token, sender.address()).await? == U256::from(50_000u64),
            "first transfer balance mismatch"
        );
        client
            .send_call(
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::updateBlocklistCall {
                    policyId: id,
                    blocked: true,
                    accounts: vec![sender.address()],
                },
                "block sender",
            )
            .await?;
        ensure!(
            !Self::is_authorized(client, id, sender.address()).await?,
            "blocked sender remained authorized"
        );
        let rejected = sender_client
            .send_call_unchecked_receipt(
                token,
                IB20::transferCall { to: recipient.address(), amount: U256::from(25_000u64) },
                "blocked transfer",
            )
            .await?;
        ensure!(!rejected.status(), "blocked transfer succeeded");
        let sender_balance = client.balance_of(token, sender.address()).await?;
        let recipient_balance = client.balance_of(token, recipient.address()).await?;
        ensure!(
            sender_balance == U256::from(50_000u64),
            "rejected transfer changed sender balance"
        );
        ensure!(
            recipient_balance == U256::from(50_000u64),
            "rejected transfer changed recipient balance"
        );
        Ok(
            json!({ "token": token, "policy_id": id, "sender": sender.address(), "accepted_transaction": accepted.transaction_hash(), "rejected_transaction": rejected.transaction_hash(), "sender_balance": sender_balance, "recipient_balance": recipient_balance }),
        )
    }

    /// Verifies built-in `ALWAYS_BLOCK` authorization and transfer rejection.
    pub async fn always_block_transfer(
        client: &B20PrecompileClient<'_>,
        admin: &PrivateKeySigner,
        recipient: &PrivateKeySigner,
    ) -> Result<Value> {
        Self::activate_policy_transfers(client).await?;
        let id = PolicyRegistryStorage::ALWAYS_BLOCK_ID;
        ensure!(
            !Self::is_authorized(client, id, admin.address()).await?,
            "ALWAYS_BLOCK authorized admin"
        );
        ensure!(
            !Self::is_authorized(client, id, recipient.address()).await?,
            "ALWAYS_BLOCK authorized arbitrary account"
        );
        ensure!(Self::policy_exists(client, id).await?, "ALWAYS_BLOCK policy missing");
        let token = Self::create_policy_token(
            client,
            admin.address(),
            B256::repeat_byte(0x52),
            "Blocked Token",
            "BLKD",
        )
        .await?;
        Self::set_transfer_policy(client, token, id).await?;
        let before_admin = client.balance_of(token, admin.address()).await?;
        let before_recipient = client.balance_of(token, recipient.address()).await?;
        let rejected = client
            .send_call_unchecked_receipt(
                token,
                IB20::transferCall { to: recipient.address(), amount: U256::from(100_000u64) },
                "ALWAYS_BLOCK transfer",
            )
            .await?;
        ensure!(!rejected.status(), "ALWAYS_BLOCK transfer succeeded");
        ensure!(
            client.balance_of(token, admin.address()).await? == before_admin,
            "rejected transfer changed admin balance"
        );
        ensure!(
            client.balance_of(token, recipient.address()).await? == before_recipient,
            "rejected transfer changed recipient balance"
        );
        Ok(
            json!({ "token": token, "policy_id": id, "admin_authorized": false, "recipient_authorized": false, "transaction_hash": rejected.transaction_hash(), "status": rejected.status(), "admin_balance": before_admin, "recipient_balance": before_recipient }),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Receipt, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom, Bytes, Log, LogData};
    use alloy_transport::TransportErrorKind;
    use base_common_consensus::BaseReceipt;
    use base_common_rpc_types::{BaseLogResponse, BaseTransactionReceipt};

    use super::RegistryWorkload;

    fn receipt_with_log(emitter: Address, data: LogData) -> BaseTransactionReceipt {
        let log = alloy_rpc_types_eth::Log {
            inner: Log { address: emitter, data },
            ..Default::default()
        };
        BaseTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: BaseReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 21_000,
                        logs: vec![BaseLogResponse::from(log)],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: B256::ZERO,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(1),
                gas_used: 21_000,
                effective_gas_price: 1,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: None,
                contract_address: None,
            },
            l1_block_info: Default::default(),
            payer: None,
            phase_statuses: None,
            metadata: None,
        }
    }

    #[test]
    fn exact_log_rejects_correct_topics_from_wrong_emitter_or_data() {
        let expected = LogData::new_unchecked(vec![B256::repeat_byte(1)], Bytes::from([2]));
        let matching = receipt_with_log(Address::repeat_byte(4), expected.clone());
        assert!(
            RegistryWorkload::require_log(&matching, Address::repeat_byte(4), &expected).is_ok()
        );
        let wrong_emitter = receipt_with_log(Address::repeat_byte(3), expected.clone());
        assert!(
            RegistryWorkload::require_log(&wrong_emitter, Address::repeat_byte(4), &expected)
                .is_err()
        );

        let mismatched = LogData::new_unchecked(expected.topics().to_vec(), Bytes::from([9]));
        let wrong_data = receipt_with_log(Address::repeat_byte(4), mismatched);
        assert!(
            RegistryWorkload::require_log(&wrong_data, Address::repeat_byte(4), &expected).is_err()
        );
    }

    #[test]
    fn network_failure_is_not_an_execution_revert() {
        let transport = TransportErrorKind::custom_str("scripted network failure");
        let error = eyre::Report::new(transport).wrap_err("activation call failed");

        assert!(!RegistryWorkload::is_execution_revert(&error));
    }

    #[test]
    fn only_execution_revert_rpc_code_satisfies_negative_assertions() {
        for (code, expected) in [(3, true), (-32601, false), (-32000, false)] {
            let error: alloy_transport::TransportError = alloy_transport::RpcError::ErrorResp(
                serde_json::from_value(serde_json::json!({
                    "code": code,
                    "message": "execution reverted"
                }))
                .unwrap(),
            );
            let error = eyre::Report::new(error).wrap_err("activation call failed");
            assert_eq!(RegistryWorkload::is_execution_revert(&error), expected);
        }
    }
}
