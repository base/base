//! EIP-4337 Account Abstraction RPC API

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolEvent, SolValue};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth::rpc::eth::EthFilter;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};
use reth_rpc_eth_api::{
    helpers::EthApiSpec, types::FullEthApiTypes, EthApiTypes, EthFilterApiServer, RpcNodeCoreExt,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// EntryPoint v0.6 address
const ENTRYPOINT_V06: Address = address!("5ff137d4b0fdcd49dca30c7cf57e578a026d2789");

/// EntryPoint v0.7 address
const ENTRYPOINT_V07: Address = address!("0000000071727de22e5e9d8baf0edac6f37da032");

/// EntryPoint v0.8 address
const ENTRYPOINT_V08: Address = address!("4337084d9e255ff0702461cf8895ce9e3b5ff108");

// Define EntryPoint UserOperation struct for ABI encoding
sol! {
    /// UserOperation struct for v0.6 (matches EntryPoint ABI)
    #[derive(Debug)]
    struct UserOperationStruct {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        uint256 callGasLimit;
        uint256 verificationGasLimit;
        uint256 preVerificationGas;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        bytes paymasterAndData;
        bytes signature;
    }
}

// Define UserOperationEvent for v0.6
sol! {
    /// UserOperationEvent emitted by EntryPoint v0.6
    #[derive(Debug)]
    event UserOperationEvent(
        bytes32 indexed userOpHash,
        address indexed sender,
        address indexed paymaster,
        uint256 nonce,
        bool success,
        uint256 actualGasCost,
        uint256 actualGasUsed
    );
}

// Define UserOperationEvent for v0.7
sol! {
    /// UserOperationEvent emitted by EntryPoint v0.7
    #[derive(Debug)]
    event UserOperationEventV07(
        bytes32 indexed userOpHash,
        address indexed sender,
        address indexed paymaster,
        uint256 nonce,
        bool success,
        uint256 actualGasCost,
        uint256 actualGasUsed
    );
}

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

impl UserOperation {
    /// Get the sender address
    pub fn sender(&self) -> Address {
        match self {
            UserOperation::V06(op) => op.sender,
            UserOperation::V07(op) => op.sender,
        }
    }
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

    /// Bundle and submit pending UserOperations to the EntryPoint
    /// This is a convenience method for testing - in production, bundling happens automatically
    #[method(name = "bundleNow")]
    async fn bundle_now(&self) -> RpcResult<B256>;
}

/// Implementation of the account abstraction RPC API
pub struct AccountAbstractionApiImpl<Provider, Eth>
where
    Eth: EthApiTypes,
{
    provider: Provider,
    eth_filter: EthFilter<Eth>,
    /// Pending UserOperations waiting to be bundled
    pending_ops: Arc<RwLock<Vec<(UserOperation, Address)>>>,
}

impl<Provider, Eth> AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec> + Clone,
    Eth: EthApiTypes,
{
    /// Creates a new instance of AccountAbstractionApi
    pub fn new(provider: Provider, eth_filter: EthFilter<Eth>) -> Self {
        Self { 
            provider, 
            eth_filter,
            pending_ops: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Get a clone of the pending operations Arc (for sharing with base API)
    pub fn get_pending_ops(&self) -> Arc<RwLock<Vec<(UserOperation, Address)>>> {
        self.pending_ops.clone()
    }

    /// Calculate the hash of a UserOperation v0.6
    fn calculate_user_op_hash_v06(
        user_op: &UserOperationV06,
        entry_point: Address,
        chain_id: u64,
    ) -> B256 {
        // Hash dynamic fields
        let hash_init_code = keccak256(&user_op.init_code);
        let hash_call_data = keccak256(&user_op.call_data);
        let hash_paymaster_and_data = keccak256(&user_op.paymaster_and_data);

        // Pack the user operation
        let packed = (
            user_op.sender,
            user_op.nonce,
            hash_init_code,
            hash_call_data,
            user_op.call_gas_limit,
            user_op.verification_gas_limit,
            user_op.pre_verification_gas,
            user_op.max_fee_per_gas,
            user_op.max_priority_fee_per_gas,
            hash_paymaster_and_data,
        );

        // Hash the packed structure
        let packed_hash = keccak256(packed.abi_encode());

        // Add entry point and chain ID context
        let final_data = (packed_hash, entry_point, U256::from(chain_id));
        keccak256(final_data.abi_encode())
    }
}

#[async_trait]
impl<Provider, Eth> AccountAbstractionApiServer for AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt
        + Clone
        + Send
        + Sync
        + 'static,
    Eth: FullEthApiTypes + EthApiSpec + RpcNodeCoreExt + Send + Sync + 'static,
{
    async fn send_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<B256> {
        let chain_id = self.provider.chain_spec().chain.id();
        
        let user_op_hash = match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    nonce = %op.nonce,
                    "Received sendUserOperation (v0.6)"
                );
                
                // Calculate the hash
                let hash = Self::calculate_user_op_hash_v06(op, entry_point, chain_id);
                
                info!(
                    hash = %hash,
                    "Calculated UserOperation hash"
                );
                
                hash
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received sendUserOperation (v0.7+)"
                );
                // TODO: Implement v0.7 hash calculation
                warn!("v0.7 UserOperations not yet fully supported");
                B256::default()
            }
        };

        // Store the UserOperation for bundling
        {
            let mut pending = self.pending_ops.write().await;
            pending.push((user_operation.clone(), entry_point));
            info!(
                pending_count = pending.len(),
                "UserOperation added to pending pool"
            );
        }

        info!(
            user_op_hash = %user_op_hash,
            "UserOperation accepted, will be bundled soon"
        );

        Ok(user_op_hash)
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

        // Validate hash
        if user_operation_hash == B256::ZERO {
            return Ok(None);
        }

        // Try each entry point (v0.6, v0.7, v0.8)
        for entry_point in [ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08] {
            debug!(
                hash = %user_operation_hash,
                entry_point = %entry_point,
                "Searching for UserOperation in EntryPoint"
            );

            // Try to find the receipt for this entry point
            if let Some(receipt) = self.get_receipt_from_entry_point(entry_point, user_operation_hash).await? {
                return Ok(Some(receipt));
            }
        }

        // Not found in any entry point
        Ok(None)
    }

    async fn supported_entry_points(&self) -> RpcResult<Vec<Address>> {
        info!("Received supportedEntryPoints request");

        Ok(vec![ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08])
    }
}

impl<Provider, Eth> AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt
        + Clone
        + Send
        + Sync
        + 'static,
    Eth: FullEthApiTypes + EthApiSpec + RpcNodeCoreExt + Send + Sync + 'static,
{
    /// Helper method to get receipt from a specific entry point
    async fn get_receipt_from_entry_point(
        &self,
        entry_point: Address,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationReceipt>> {
        // For MVP, we'll scan recent blocks for the UserOperationEvent
        // In production, you'd want to index these events or use a more efficient lookup
        
        // Get the latest block number
        let latest_block = match self.provider.last_block_number() {
            Ok(num) => num,
            Err(e) => {
                debug!(error = ?e, "Failed to get latest block number");
                return Ok(None);
            }
        };

        // For MVP, search the last 1000 blocks
        // TODO: In production, use an event index or database
        let from_block = latest_block.saturating_sub(1000);
        
        debug!(
            from_block = from_block,
            to_block = latest_block,
            entry_point = %entry_point,
            "Searching blocks for UserOperationEvent"
        );

        // Create a filter for UserOperationEvent logs
        // The event signature is the same for v0.6 and v0.7
        let event_signature = UserOperationEvent::SIGNATURE_HASH;
        
        let filter = alloy_rpc_types_eth::Filter::default()
            .address(entry_point)
            .event_signature(event_signature)
            .topic1(user_operation_hash) // userOpHash is the first indexed parameter
            .from_block(from_block)
            .to_block(latest_block);

        // Query logs from the filter
        let logs = match self.eth_filter.logs(filter).await {
            Ok(logs) => logs,
            Err(e) => {
                warn!(error = ?e, "Failed to query logs");
                return Ok(None);
            }
        };

        debug!(log_count = logs.len(), "Found matching logs");

        // Find the log matching our user operation hash
        let log = match logs.into_iter().find(|log| {
            // The userOpHash is topic1 (first indexed parameter after event signature)
            log.topics().get(1).map(|t| *t == user_operation_hash).unwrap_or(false)
        }) {
            Some(log) => log,
            None => {
                debug!("No matching UserOperationEvent found");
                return Ok(None);
            }
        };

        // Parse the event data
        let event = match UserOperationEvent::decode_log(&log.inner) {
            Ok(event) => event.data,
            Err(e) => {
                warn!(error = ?e, "Failed to decode UserOperationEvent");
                return Ok(None);
            }
        };

        debug!(
            sender = %event.sender,
            nonce = %event.nonce,
            success = event.success,
            actual_gas_cost = %event.actualGasCost,
            actual_gas_used = %event.actualGasUsed,
            "Decoded UserOperationEvent"
        );

        // Build the UserOperationReceipt
        // For MVP, we return a minimal receipt with the event data
        let receipt = UserOperationReceipt {
            user_op_hash: user_operation_hash,
            entry_point,
            sender: event.sender,
            nonce: event.nonce,
            paymaster: event.paymaster,
            actual_gas_cost: event.actualGasCost,
            actual_gas_used: event.actualGasUsed,
            success: event.success,
            // TODO: Get actual receipt data from the transaction
            // For now, serialize the log data as bytes
            receipt: Bytes::default(),
            logs: vec![],  // TODO: Serialize logs properly
        };

        Ok(Some(receipt))
    }
}

/// Implementation of the base account abstraction RPC API
pub struct BaseAccountAbstractionApiImpl<Provider>
{
    provider: Provider,
    pending_ops: Arc<RwLock<Vec<(UserOperation, Address)>>>,
}

impl<Provider> BaseAccountAbstractionApiImpl<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec> + Clone,
{
    /// Creates a new instance of BaseAccountAbstractionApi
    pub fn new(
        provider: Provider,
        pending_ops: Arc<RwLock<Vec<(UserOperation, Address)>>>,
    ) -> Self {
        Self {
            provider,
            pending_ops,
        }
    }
}

#[async_trait]
impl<Provider> BaseAccountAbstractionApiServer
    for BaseAccountAbstractionApiImpl<Provider>
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

    async fn bundle_now(&self) -> RpcResult<B256> {
        info!("Received bundle_now request");

        // Get all pending UserOperations
        let pending_ops = {
            let mut pending = self.pending_ops.write().await;
            let ops = pending.drain(..).collect::<Vec<_>>();
            ops
        };

        if pending_ops.is_empty() {
            info!("No pending UserOperations to bundle");
            return Ok(B256::ZERO);
        }

        info!(
            count = pending_ops.len(),
            "Bundling UserOperations"
        );

        // For MVP, just log what we would do
        // In production, you would:
        // 1. Create a transaction calling EntryPoint.handleOps()
        // 2. Sign it with the bundler's key
        // 3. Submit it to the network
        // 4. Return the transaction hash

        for (user_op, entry_point) in &pending_ops {
            info!(
                sender = %user_op.sender(),
                entry_point = %entry_point,
                "Would bundle UserOperation"
            );
        }

        warn!("Bundle creation not yet implemented - UserOperations logged only");
        warn!("To implement: create tx calling EntryPoint.handleOps() with pending ops");

        // Return a dummy hash for now
        Ok(B256::ZERO)
    }
}

