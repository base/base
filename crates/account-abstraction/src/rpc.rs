//! EIP-4337 Account Abstraction RPC API

use alloy_primitives::{address, Address, Bytes, B256, U256};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use serde::{Deserialize, Serialize};
use tracing::info;

/// EntryPoint v0.6 address
const ENTRYPOINT_V06: Address = address!("5ff137d4b0fdcd49dca30c7cf57e578a026d2789");

/// EntryPoint v0.7 address
const ENTRYPOINT_V07: Address = address!("0000000071727de22e5e9d8baf0edac6f37da032");

/// EntryPoint v0.8 address
const ENTRYPOINT_V08: Address = address!("4337084d9e255ff0702461cf8895ce9e3b5ff108");

/// User Operation as defined by EIP-4337 v0.6
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationV06 {
    pub sender: Address,
    pub nonce: U256,
    pub init_code: Bytes,
    pub call_data: Bytes,
    pub call_gas_limit: U256,
    pub verification_gas_limit: U256,
    pub pre_verification_gas: U256,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub paymaster_and_data: Bytes,
    pub signature: Bytes,
}

/// User Operation as defined by EIP-4337 v0.7+
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationV07 {
    pub sender: Address,
    pub nonce: U256,
    pub factory: Address,
    pub factory_data: Bytes,
    pub call_data: Bytes,
    pub call_gas_limit: U256,
    pub verification_gas_limit: U256,
    pub pre_verification_gas: U256,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub paymaster: Address,
    pub paymaster_verification_gas_limit: U256,
    pub paymaster_post_op_gas_limit: U256,
    pub paymaster_data: Bytes,
    pub signature: Bytes,
}

/// User Operation that can be either v0.6 or v0.7+
/// Automatically deserializes based on fields present
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserOperation {
    V06(UserOperationV06),
    V07(UserOperationV07),
}

/// Packed User Operation (on-chain format for v0.7+ EntryPoint)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackedUserOperation {
    pub sender: Address,
    pub nonce: U256,
    pub init_code: Bytes,
    pub call_data: Bytes,
    pub account_gas_limits: Bytes,
    pub pre_verification_gas: U256,
    pub gas_fees: Bytes,
    pub paymaster_and_data: Bytes,
    pub signature: Bytes,
}

/// Gas estimates for a User Operation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationGasEstimate {
    pub pre_verification_gas: U256,
    pub verification_gas_limit: U256,
    pub call_gas_limit: U256,
}

/// User Operation with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationWithMetadata {
    pub user_operation: UserOperation,
    pub entry_point: Address,
    pub block_number: u64,
    pub block_hash: B256,
    pub transaction_hash: B256,
}

/// User Operation receipt
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationReceipt {
    pub user_op_hash: B256,
    pub entry_point: Address,
    pub sender: Address,
    pub nonce: U256,
    pub paymaster: Address,
    pub actual_gas_cost: U256,
    pub actual_gas_used: U256,
    pub success: bool,
    pub logs: Vec<Bytes>,
    pub receipt: Bytes,
}

/// Validation result for User Operations
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    pub valid: bool,
    pub reason: Option<String>,
}

/// RPC API for EIP-4337 account abstraction (supports v0.6 and v0.7+)
#[rpc(server, namespace = "eth")]
pub trait AccountAbstractionApi {
    /// Submits a User Operation to the bundler pool
    #[method(name = "sendUserOperation")]
    async fn send_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<B256>;

    /// Estimates gas values for a User Operation
    #[method(name = "estimateUserOperationGas")]
    async fn estimate_user_operation_gas(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<UserOperationGasEstimate>;

    /// Returns a User Operation by its hash
    #[method(name = "getUserOperationByHash")]
    async fn get_user_operation_by_hash(
        &self,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationWithMetadata>>;

    /// Returns the receipt of a User Operation
    #[method(name = "getUserOperationReceipt")]
    async fn get_user_operation_receipt(
        &self,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationReceipt>>;

    /// Returns supported entry point addresses
    #[method(name = "supportedEntryPoints")]
    async fn supported_entry_points(&self) -> RpcResult<Vec<Address>>;
}

/// Base namespace RPC API for account abstraction (supports v0.6 and v0.7+)
#[rpc(server, namespace = "base")]
pub trait BaseAccountAbstractionApi {
    /// Validates a User Operation without submitting it
    #[method(name = "validateUserOperation")]
    async fn validate_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<ValidationResult>;
}

/// Implementation of the account abstraction RPC API
pub struct AccountAbstractionApiImpl<Provider> {
    provider: Provider,
}

impl<Provider> AccountAbstractionApiImpl<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec> + Clone,
{
    /// Creates a new instance of AccountAbstractionApi
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<Provider> AccountAbstractionApiServer for AccountAbstractionApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn send_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<B256> {
        match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received sendUserOperation request (v0.6)"
                );
                // TODO: Validate v0.6 user operation
                // TODO: Submit to bundler pool
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received sendUserOperation request (v0.7+)"
                );
                // TODO: Validate v0.7 user operation
                // TODO: Convert to PackedUserOperation for on-chain submission
                // TODO: Submit to bundler pool
            }
        }

        // TODO: Return user operation hash
        Ok(B256::default())
    }

    async fn estimate_user_operation_gas(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<UserOperationGasEstimate> {
        match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received estimateUserOperationGas request (v0.6)"
                );
                // TODO: Simulate v0.6 user operation
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received estimateUserOperationGas request (v0.7+)"
                );
                // TODO: Convert to PackedUserOperation for simulation
                // TODO: Simulate v0.7 user operation
            }
        }

        // TODO: Estimate gas requirements
        Ok(UserOperationGasEstimate {
            pre_verification_gas: U256::from(21000),
            verification_gas_limit: U256::from(100000),
            call_gas_limit: U256::from(100000),
        })
    }

    async fn get_user_operation_by_hash(
        &self,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationWithMetadata>> {
        info!(
            hash = %user_operation_hash,
            "Received getUserOperationByHash request"
        );

        // TODO: Lookup user operation from storage (will be either v0.6 or v0.7)

        Ok(None)
    }

    async fn get_user_operation_receipt(
        &self,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationReceipt>> {
        info!(
            hash = %user_operation_hash,
            "Received getUserOperationReceipt request"
        );

        // TODO: Lookup receipt from storage

        Ok(None)
    }

    async fn supported_entry_points(&self) -> RpcResult<Vec<Address>> {
        info!("Received supportedEntryPoints request");

        Ok(vec![ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08])
    }
}

/// Implementation of the base account abstraction RPC API
pub struct BaseAccountAbstractionApiImpl<Provider> {
    provider: Provider,
}

impl<Provider> BaseAccountAbstractionApiImpl<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec> + Clone,
{
    /// Creates a new instance of BaseAccountAbstractionApi
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<Provider> BaseAccountAbstractionApiServer for BaseAccountAbstractionApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn validate_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<ValidationResult> {
        match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received validateUserOperation request (v0.6)"
                );
                // TODO: Validate user operation per EIP-4337 v0.6
                // TODO: Check signature, nonce, gas limits
                // TODO: Simulate validation on entry point
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received validateUserOperation request (v0.7+)"
                );
                // TODO: Convert to PackedUserOperation for validation
                // TODO: Validate user operation per EIP-4337 v0.7+
                // TODO: Check signature, nonce, gas limits
                // TODO: Simulate validation on entry point
            }
        }

        Ok(ValidationResult {
            valid: true,
            reason: None,
        })
    }
}

