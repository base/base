use alloy_primitives::{Address, Bytes, B256, U256};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use serde::{Deserialize, Serialize};
use tracing::info;

/// User Operation as defined by EIP-4337
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperation {
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

/// RPC API for EIP-4337 account abstraction
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

/// Base namespace RPC API for account abstraction
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
        info!(
            sender = %user_operation.sender,
            entry_point = %entry_point,
            "Received sendUserOperation request"
        );

        // TODO: Validate user operation
        // TODO: Submit to bundler pool
        // TODO: Return user operation hash

        Ok(B256::default())
    }

    async fn estimate_user_operation_gas(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<UserOperationGasEstimate> {
        info!(
            sender = %user_operation.sender,
            entry_point = %entry_point,
            "Received estimateUserOperationGas request"
        );

        // TODO: Simulate user operation
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

        // TODO: Lookup user operation from storage

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

        // TODO: Return configured entry points

        Ok(Vec::new())
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
        info!(
            sender = %user_operation.sender,
            entry_point = %entry_point,
            "Received validateUserOperation request"
        );

        // TODO: Validate user operation per EIP-4337
        // TODO: Check signature, nonce, gas limits
        // TODO: Simulate validation on entry point

        Ok(ValidationResult {
            valid: true,
            reason: None,
        })
    }
}

