//! EIP-4337 Account Abstraction RPC API

use std::sync::Arc;

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::state::StateOverride;
use alloy_rpc_types_eth::Log;
use base_account_abstraction_indexer::UserOperationStorage;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use op_alloy_network::Optimism;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{BlockNumReader, BlockReader, ChainSpecProvider, ReceiptProvider, StateProviderFactory};
use reth_rpc_eth_api::helpers::{FullEthApi, TraceExt};
use reth_rpc_eth_api::EthApiTypes;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, info, warn};
use url::Url;

/// Custom deserializer for optional Address fields that accepts:
/// - "0x" or "0x0" → Address::ZERO
/// - Full 20-byte hex address
/// - null → Address::ZERO
/// - Omitted field → Address::ZERO (via #[serde(default)])
fn deserialize_optional_address<'de, D>(deserializer: D) -> Result<Address, D::Error>
where
    D: Deserializer<'de>,
{
    // First try to deserialize as Option<String> to handle null values
    let opt: Option<String> = Deserialize::deserialize(deserializer)?;
    
    match opt {
        None => Ok(Address::ZERO),
        Some(s) => {
            // Handle empty or short hex strings as zero address
            if s == "0x" || s == "0x0" || s.is_empty() {
                return Ok(Address::ZERO);
            }
            // Try to parse as a proper address
            s.parse::<Address>().map_err(serde::de::Error::custom)
        }
    }
}

use crate::config::AccountAbstractionArgs;
use crate::entrypoint::{get_entrypoint_version, EntryPointVersion};
use crate::estimation::{
    GasEstimationConfig, GasEstimationError, GasEstimationResult, GasEstimator,
    RethPreVerificationGasCalculator,
};
use crate::provider::{
    EthApiSimulationProvider, RethReceiptProvider, UserOperationReceiptProvider,
};
use crate::tips_client::TipsClient;

/// User Operation as defined by EIP-4337 v0.6
/// Gas and fee fields are optional for estimation - they default to 0 when not provided
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationV06 {
    pub sender: Address,
    pub nonce: U256,
    pub init_code: Bytes,
    pub call_data: Bytes,
    #[serde(default)]
    pub call_gas_limit: U256,
    #[serde(default)]
    pub verification_gas_limit: U256,
    #[serde(default)]
    pub pre_verification_gas: U256,
    #[serde(default)]
    pub max_fee_per_gas: U256,
    #[serde(default)]
    pub max_priority_fee_per_gas: U256,
    pub paymaster_and_data: Bytes,
    pub signature: Bytes,
}

/// User Operation as defined by EIP-4337 v0.7+
/// Gas and fee fields are optional for estimation - they default to 0 when not provided
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationV07 {
    pub sender: Address,
    pub nonce: U256,
    #[serde(default, deserialize_with = "deserialize_optional_address")]
    pub factory: Address,
    #[serde(default)]
    pub factory_data: Bytes,
    pub call_data: Bytes,
    #[serde(default)]
    pub call_gas_limit: U256,
    #[serde(default)]
    pub verification_gas_limit: U256,
    #[serde(default)]
    pub pre_verification_gas: U256,
    #[serde(default)]
    pub max_fee_per_gas: U256,
    #[serde(default)]
    pub max_priority_fee_per_gas: U256,
    #[serde(default, deserialize_with = "deserialize_optional_address")]
    pub paymaster: Address,
    #[serde(default)]
    pub paymaster_verification_gas_limit: U256,
    #[serde(default)]
    pub paymaster_post_op_gas_limit: U256,
    #[serde(default)]
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
    /// Compute the UserOperation hash as defined by EIP-4337
    ///
    /// The hash is computed as: `keccak256(abi.encode(pack(userOp), entryPoint, chainId))`
    pub fn hash(&self, entry_point: Address, chain_id: u64) -> B256 {
        use alloy_primitives::keccak256;
        use alloy_sol_types::SolValue;

        let inner_hash = match self {
            UserOperation::V06(op) => {
                // For v0.6, hash the packed UserOp struct
                // keccak256(abi.encode(sender, nonce, keccak256(initCode), keccak256(callData),
                //   callGasLimit, verificationGasLimit, preVerificationGas, maxFeePerGas,
                //   maxPriorityFeePerGas, keccak256(paymasterAndData)))
                let init_code_hash = keccak256(&op.init_code);
                let call_data_hash = keccak256(&op.call_data);
                let paymaster_data_hash = keccak256(&op.paymaster_and_data);

                let encoded = (
                    op.sender,
                    op.nonce,
                    init_code_hash,
                    call_data_hash,
                    op.call_gas_limit,
                    op.verification_gas_limit,
                    op.pre_verification_gas,
                    op.max_fee_per_gas,
                    op.max_priority_fee_per_gas,
                    paymaster_data_hash,
                )
                    .abi_encode();
                keccak256(&encoded)
            }
            UserOperation::V07(op) => {
                // For v0.7, hash the packed UserOp struct
                // Pack initCode = factory + factoryData
                let init_code = if op.factory != Address::ZERO {
                    let mut ic = op.factory.as_slice().to_vec();
                    ic.extend_from_slice(&op.factory_data);
                    Bytes::from(ic)
                } else {
                    Bytes::default()
                };

                // Pack accountGasLimits = verificationGasLimit (16 bytes) | callGasLimit (16 bytes)
                let mut account_gas_limits = [0u8; 32];
                // U256 to_be_bytes gives 32 bytes, we need the lower 16 bytes
                let ver_gas_bytes = op.verification_gas_limit.to_be_bytes::<32>();
                account_gas_limits[..16].copy_from_slice(&ver_gas_bytes[16..32]);
                let call_gas_bytes = op.call_gas_limit.to_be_bytes::<32>();
                account_gas_limits[16..].copy_from_slice(&call_gas_bytes[16..32]);

                // Pack gasFees = maxPriorityFeePerGas (16 bytes) | maxFeePerGas (16 bytes)
                let mut gas_fees = [0u8; 32];
                let priority_bytes = op.max_priority_fee_per_gas.to_be_bytes::<32>();
                gas_fees[..16].copy_from_slice(&priority_bytes[16..32]);
                let max_fee_bytes = op.max_fee_per_gas.to_be_bytes::<32>();
                gas_fees[16..].copy_from_slice(&max_fee_bytes[16..32]);

                // Pack paymasterAndData
                let paymaster_and_data = if op.paymaster != Address::ZERO {
                    let mut pad = op.paymaster.as_slice().to_vec();
                    // Pack verification gas limit (16 bytes)
                    let pm_ver_bytes = op.paymaster_verification_gas_limit.to_be_bytes::<32>();
                    pad.extend_from_slice(&pm_ver_bytes[16..32]);
                    // Pack post op gas limit (16 bytes)
                    let pm_post_bytes = op.paymaster_post_op_gas_limit.to_be_bytes::<32>();
                    pad.extend_from_slice(&pm_post_bytes[16..32]);
                    // Append paymaster data
                    pad.extend_from_slice(&op.paymaster_data);
                    Bytes::from(pad)
                } else {
                    Bytes::default()
                };

                let init_code_hash = keccak256(&init_code);
                let call_data_hash = keccak256(&op.call_data);
                let paymaster_data_hash = keccak256(&paymaster_and_data);

                let encoded = (
                    op.sender,
                    op.nonce,
                    init_code_hash,
                    call_data_hash,
                    B256::from(account_gas_limits),
                    op.pre_verification_gas,
                    B256::from(gas_fees),
                    paymaster_data_hash,
                )
                    .abi_encode();
                keccak256(&encoded)
            }
        };

        // Final hash = keccak256(abi.encode(innerHash, entryPoint, chainId))
        let final_encoded = (inner_hash, entry_point, U256::from(chain_id)).abi_encode();
        keccak256(&final_encoded)
    }

    /// Get the sender address
    pub fn sender(&self) -> Address {
        match self {
            UserOperation::V06(op) => op.sender,
            UserOperation::V07(op) => op.sender,
        }
    }

    /// Get the factory address if present
    pub fn factory(&self) -> Option<Address> {
        match self {
            UserOperation::V06(op) => {
                if op.init_code.len() >= 20 {
                    Some(Address::from_slice(&op.init_code[..20]))
                } else {
                    None
                }
            }
            UserOperation::V07(op) => {
                if op.factory != Address::ZERO {
                    Some(op.factory)
                } else {
                    None
                }
            }
        }
    }

    /// Get the paymaster address if present
    pub fn paymaster(&self) -> Option<Address> {
        match self {
            UserOperation::V06(op) => {
                if op.paymaster_and_data.len() >= 20 {
                    Some(Address::from_slice(&op.paymaster_and_data[..20]))
                } else {
                    None
                }
            }
            UserOperation::V07(op) => {
                if op.paymaster != Address::ZERO {
                    Some(op.paymaster)
                } else {
                    None
                }
            }
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

/// Gas estimates for a User Operation (v0.6 compatible)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationGasEstimate {
    pub pre_verification_gas: U256,
    pub verification_gas_limit: U256,
    pub call_gas_limit: U256,
    /// (v0.7+) Gas limit for paymaster verification, only present for v0.7+ operations with paymaster
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster_verification_gas_limit: Option<U256>,
    /// (v0.7+) Gas limit for paymaster post-op, only present for v0.7+ operations with paymaster
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster_post_op_gas_limit: Option<U256>,
}

impl From<GasEstimationResult> for UserOperationGasEstimate {
    fn from(result: GasEstimationResult) -> Self {
        Self {
            pre_verification_gas: result.pre_verification_gas,
            verification_gas_limit: result.verification_gas_limit,
            call_gas_limit: result.call_gas_limit,
            paymaster_verification_gas_limit: result.paymaster_verification_gas_limit,
            paymaster_post_op_gas_limit: result.paymaster_post_op_gas_limit,
        }
    }
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
    /// Revert reason if the operation failed (empty if success)
    pub reason: Bytes,
    pub logs: Vec<Log>,
    pub receipt: TransactionReceiptInfo,
}

/// Transaction receipt info included in UserOperationReceipt
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceiptInfo {
    pub transaction_hash: B256,
    pub transaction_index: u64,
    pub block_hash: B256,
    pub block_number: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub cumulative_gas_used: U256,
    pub gas_used: U256,
    pub effective_gas_price: U256,
}

/// Validation result for User Operations
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    /// Whether the UserOp is valid
    pub valid: bool,
    /// Error message if not valid
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Timestamp until the UserOp is valid (0 = no expiry)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<u64>,
    /// Timestamp after which the UserOp is valid (0 = immediately)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_after: Option<u64>,
    /// Entity stake/deposit context
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ValidationContext>,
}

/// Entity stake/deposit information context
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationContext {
    /// Sender (account) stake info
    pub sender_info: EntityStakeInfo,
    /// Factory stake info (if present)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_info: Option<EntityStakeInfo>,
    /// Paymaster stake info (if present)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster_info: Option<EntityStakeInfo>,
    /// Aggregator stake info (if present)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator_info: Option<AggregatorInfo>,
    /// Code hashes for all entities at validation time
    ///
    /// Used for:
    /// - COD-010: Detecting code changes between 1st and 2nd validation
    /// - Mempool inclusion decisions when entity code changes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hashes: Option<CodeHashes>,
}

/// Code hashes for entities (used in RPC response)
///
/// Captured at validation time to support COD-010 and mempool decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeHashes {
    /// Sender (account) code hash
    pub sender: B256,
    /// Factory code hash (if factory is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory: Option<B256>,
    /// Paymaster code hash (if paymaster is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster: Option<B256>,
    /// Aggregator code hash (if aggregator is used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator: Option<B256>,
}

/// Stake info for an entity (used in RPC response)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityStakeInfo {
    /// Entity address
    pub address: Address,
    /// Amount staked
    pub stake: U256,
    /// Unstake delay in seconds
    pub unstake_delay_sec: u64,
    /// Amount deposited for gas
    pub deposit: U256,
    /// Whether entity meets staking requirements
    pub is_staked: bool,
}

/// Aggregator stake info (used in RPC response)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregatorInfo {
    /// Aggregator address
    pub aggregator: Address,
    /// Stake info
    pub stake_info: EntityStakeInfo,
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
    /// 
    /// # Parameters
    /// - `user_operation`: The UserOperation to estimate gas for
    /// - `entry_point`: Address of the EntryPoint contract
    /// - `state_override`: Optional state overrides for the simulation (e.g., account code/balance)
    #[method(name = "estimateUserOperationGas")]
    async fn estimate_user_operation_gas(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
        state_override: Option<StateOverride>,
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

/// The handler for sending UserOperations based on configured mode
#[derive(Clone)]
pub enum UserOpSendHandler {
    /// Forward to TIPS service
    Tips(TipsClient),
    /// Store in local mempool
    Mempool {
        /// Reference to the mempool
        pool: Arc<parking_lot::RwLock<crate::mempool::UserOpPool>>,
        /// Chain ID for hash computation
        chain_id: u64,
        /// Optional gossip handle for p2p propagation
        gossip_handle: Option<crate::mempool::UserOpGossipHandle>,
    },
}

/// Implementation of the account abstraction RPC API
pub struct AccountAbstractionApiImpl<Provider, Eth> {
    provider: Provider,
    /// The Eth API for simulation (eth_call)
    eth_api: Eth,
    /// Handler for sending UserOperations (Tips or Mempool)
    send_handler: UserOpSendHandler,
    /// Receipt provider for looking up UserOperation receipts
    receipt_provider: Arc<RethReceiptProvider<Provider>>,
}

impl<Provider, Eth> AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec> + Clone,
    Eth: FullEthApi<NetworkTypes = Optimism> + Clone,
{
    /// Creates a new instance of AccountAbstractionApi in Tips mode
    ///
    /// # Arguments
    /// * `provider` - The state provider for blockchain access
    /// * `eth_api` - The Eth API for simulation calls
    /// * `tips_url` - URL of the Tips ingress service
    /// * `storage` - Optional indexed storage from the AA indexer ExEx
    /// * `args` - Account abstraction CLI arguments
    pub fn new(
        provider: Provider,
        eth_api: Eth,
        tips_url: Url,
        storage: Option<Arc<UserOperationStorage>>,
        args: &AccountAbstractionArgs,
    ) -> Self {
        let receipt_provider = Arc::new(RethReceiptProvider::new(
            provider.clone(),
            storage,
            args.user_op_event_lookback_blocks(),
        ));
        Self {
            provider,
            eth_api,
            send_handler: UserOpSendHandler::Tips(TipsClient::new(tips_url)),
            receipt_provider,
        }
    }

    /// Creates a new instance of AccountAbstractionApi in Mempool mode
    ///
    /// # Arguments
    /// * `provider` - The state provider for blockchain access
    /// * `eth_api` - The Eth API for simulation calls
    /// * `pool` - Reference to the UserOperation mempool
    /// * `chain_id` - Chain ID for UserOp hash computation
    /// * `gossip_handle` - Optional handle for p2p propagation
    /// * `storage` - Optional indexed storage from the AA indexer ExEx
    /// * `args` - Account abstraction CLI arguments
    pub fn new_with_mempool(
        provider: Provider,
        eth_api: Eth,
        pool: Arc<parking_lot::RwLock<crate::mempool::UserOpPool>>,
        chain_id: u64,
        gossip_handle: Option<crate::mempool::UserOpGossipHandle>,
        storage: Option<Arc<UserOperationStorage>>,
        args: &AccountAbstractionArgs,
    ) -> Self {
        let receipt_provider = Arc::new(RethReceiptProvider::new(
            provider.clone(),
            storage,
            args.user_op_event_lookback_blocks(),
        ));
        Self {
            provider,
            eth_api,
            send_handler: UserOpSendHandler::Mempool {
                pool,
                chain_id,
                gossip_handle,
            },
            receipt_provider,
        }
    }

    /// Get the underlying mempool if in mempool mode
    pub fn mempool(&self) -> Option<Arc<parking_lot::RwLock<crate::mempool::UserOpPool>>> {
        match &self.send_handler {
            UserOpSendHandler::Mempool { pool, .. } => Some(pool.clone()),
            UserOpSendHandler::Tips(_) => None,
        }
    }
}

impl<Provider, Eth> AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: BlockNumReader + ReceiptProvider + BlockReader + ChainSpecProvider<ChainSpec = OpChainSpec> + Send + Sync,
    Eth: Send + Sync,
{
    /// Build a UserOperationReceipt from a lookup result
    ///
    /// This method converts the provider's `UserOperationLookupResult` into the RPC response type.
    fn build_receipt_from_lookup(
        &self,
        lookup: crate::provider::UserOperationLookupResult,
    ) -> Result<UserOperationReceipt, String> {
        let indexed = &lookup.indexed;

        // Get the transaction receipt for additional details (cumulative gas)
        let tx_receipt = self
            .provider
            .receipt_by_hash(indexed.transaction_hash)
            .map_err(|e| format!("Failed to get transaction receipt: {}", e))?
            .ok_or_else(|| "Transaction receipt not found".to_string())?;

        Ok(UserOperationReceipt {
            user_op_hash: indexed.user_op_hash,
            entry_point: indexed.entry_point,
            sender: indexed.sender,
            nonce: indexed.nonce,
            paymaster: indexed.paymaster,
            actual_gas_cost: indexed.actual_gas_cost,
            actual_gas_used: indexed.actual_gas_used,
            success: indexed.success,
            reason: lookup.reason,
            logs: lookup.logs,
            receipt: TransactionReceiptInfo {
                transaction_hash: indexed.transaction_hash,
                transaction_index: indexed.transaction_index,
                block_hash: indexed.block_hash,
                block_number: indexed.block_number,
                from: Address::ZERO, // We'd need to look this up from the tx
                to: Some(indexed.entry_point),
                cumulative_gas_used: U256::from(tx_receipt.cumulative_gas_used()),
                gas_used: indexed.actual_gas_used,
                effective_gas_price: U256::ZERO, // We'd need to look this up
            },
        })
    }

    /// Extract the full UserOperation from a transaction's calldata
    ///
    /// This handles two cases:
    /// 1. Direct call to EntryPoint: decode from tx.input
    /// 2. Subcall to EntryPoint (via wrapper): would need debug_trace_transaction (not yet implemented)
    fn extract_user_operation_from_tx(
        &self,
        user_op_hash: B256,
        tx_hash: B256,
        entry_point: Address,
        block_number: u64,
        chain_id: u64,
    ) -> Result<Option<UserOperation>, jsonrpsee::types::ErrorObjectOwned> {
        use crate::contracts::{decode_user_operations_from_calldata, is_entrypoint};
        use alloy_consensus::Transaction as AlloyTx;
        use reth::api::BlockBody;
        use reth_primitives_traits::Block;

        // Get the block containing the transaction
        let block = self
            .provider
            .block(block_number.into())
            .map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    format!("Failed to get block {}: {}", block_number, e),
                    None::<String>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    format!("Block {} not found", block_number),
                    None::<String>,
                )
            })?;

        // Find the transaction in the block by hash
        let tx = block
            .body()
            .transactions()
            .iter()
            .find(|tx| {
                use alloy_consensus::transaction::TxHashRef;
                *tx.tx_hash() == tx_hash
            })
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    format!("Transaction {} not found in block {}", tx_hash, block_number),
                    None::<String>,
                )
            })?;

        // Get the `to` address - if it's the entrypoint, we can decode directly
        // Use SignedTransaction trait to access transaction data
        let tx_kind = tx.kind();
        let tx_to = tx_kind.to();

        if tx_to.is_some_and(|to| is_entrypoint(*to) || *to == entry_point) {
            // Direct call to EntryPoint - decode from calldata
            let calldata = Bytes::copy_from_slice(tx.input());
            
            debug!(
                target: "aa-rpc",
                tx_hash = %tx_hash,
                entry_point = %entry_point,
                calldata_len = calldata.len(),
                "Decoding UserOperation from direct EntryPoint call"
            );

            let user_ops = decode_user_operations_from_calldata(&calldata, entry_point);

            // Find the UserOperation that matches the hash
            for op in user_ops {
                let computed_hash = op.hash(entry_point, chain_id);
                if computed_hash == user_op_hash {
                    debug!(
                        target: "aa-rpc",
                        hash = %user_op_hash,
                        sender = %op.sender(),
                        "Found matching UserOperation"
                    );
                    return Ok(Some(op));
                }
            }

            debug!(
                target: "aa-rpc",
                hash = %user_op_hash,
                tx_hash = %tx_hash,
                "UserOperation hash not found in decoded operations"
            );
            Ok(None)
        } else {
            // Transaction is not a direct call to EntryPoint
            // This means the UserOp was submitted through a wrapper contract
            // We would need to trace the transaction to find the subcall to EntryPoint
            debug!(
                target: "aa-rpc",
                tx_hash = %tx_hash,
                tx_to = ?tx_to,
                entry_point = %entry_point,
                "Transaction is not a direct EntryPoint call, tracing required (not yet implemented)"
            );

            // TODO: Implement trace_find_user_operation for subcalls
            // This would use debug_trace_transaction with CallTracer to find the subcall
            // to the EntryPoint and decode the UserOperation from that call's input.
            // For now, return None to indicate we couldn't extract the UserOp
            Ok(None)
        }
    }
}

/// Simulation methods that require EthApi for eth_call and debug tracing
impl<Provider, Eth> AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: BlockNumReader
        + ReceiptProvider
        + BlockReader
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + Clone
        + Send
        + Sync,
    Eth: FullEthApi<NetworkTypes = Optimism> + EthApiTypes + TraceExt + Clone + Send + Sync + 'static,
{
    /// Internal validation implementation
    async fn validate_internal(
        &self,
        user_operation: &UserOperation,
        entry_point: Address,
    ) -> Result<crate::simulation::ValidationOutput, crate::simulation::ValidationError> {
        use crate::simulation::UserOperationValidator;

        let sim_provider = Arc::new(EthApiSimulationProvider::new(self.eth_api.clone()));
        let validator = UserOperationValidator::new(sim_provider);

        validator.validate(user_operation, entry_point).await
    }

    /// Trace a transaction to find a UserOperation in a subcall to the EntryPoint
    ///
    /// Uses debug_trace_transaction with CallTracer to find calls to the EntryPoint
    /// and extracts the UserOperation from the subcall's input data.
    ///
    /// This is used when the transaction's `to` is not the EntryPoint, meaning
    /// the UserOperation was submitted through a wrapper contract.
    async fn trace_find_user_operation(
        &self,
        tx_hash: B256,
        user_op_hash: B256,
        entry_point: Address,
        chain_id: u64,
    ) -> Result<Option<UserOperation>, jsonrpsee::types::ErrorObjectOwned> {
        use crate::contracts::{decode_user_operations_from_calldata, is_entrypoint};
        use alloy_rpc_types_trace::geth::{
            CallFrame, GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingOptions,
            GethTrace,
        };
        use reth_rpc::DebugApi;
        use reth_tasks::pool::BlockingTaskGuard;
        use std::collections::VecDeque;

        debug!(
            target: "aa-rpc",
            tx_hash = %tx_hash,
            user_op_hash = %user_op_hash,
            entry_point = %entry_point,
            "Tracing transaction to find UserOperation in subcall"
        );

        // Create a DebugApi instance for tracing
        let blocking_task_guard = BlockingTaskGuard::new(4);
        let debug_api = DebugApi::new(self.eth_api.clone(), blocking_task_guard);

        // Set up tracing options with CallTracer
        let trace_options = GethDebugTracingOptions {
            tracer: Some(GethDebugTracerType::BuiltInTracer(
                GethDebugBuiltInTracerType::CallTracer,
            )),
            ..Default::default()
        };

        // Trace the transaction
        let trace = debug_api
            .debug_trace_transaction(tx_hash, trace_options)
            .await
            .map_err(|e| {
                warn!(
                    target: "aa-rpc",
                    tx_hash = %tx_hash,
                    error = %e,
                    "Failed to trace transaction"
                );
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    format!("Failed to trace transaction: {}", e),
                    None::<String>,
                )
            })?;

        // Breadth-first search through the call trace for calls to EntryPoint
        let mut frame_queue = VecDeque::new();
        if let GethTrace::CallTracer(call_frame) = trace {
            frame_queue.push_back(call_frame);
        } else {
            debug!(
                target: "aa-rpc",
                tx_hash = %tx_hash,
                "Trace result is not a CallTracer frame"
            );
            return Ok(None);
        }

        while let Some(call_frame) = frame_queue.pop_front() {
            // Check if this call is to an EntryPoint
            if let Some(to) = call_frame.to {
                if is_entrypoint(to) || to == entry_point {
                    // Found a call to EntryPoint - try to decode UserOperations from its input
                    let user_ops =
                        decode_user_operations_from_calldata(&call_frame.input, entry_point);

                    // Find the UserOperation that matches the hash
                    for op in user_ops {
                        let computed_hash = op.hash(entry_point, chain_id);
                        if computed_hash == user_op_hash {
                            debug!(
                                target: "aa-rpc",
                                hash = %user_op_hash,
                                sender = %op.sender(),
                                "Found matching UserOperation in traced subcall"
                            );
                            return Ok(Some(op));
                        }
                    }
                }
            }

            // Enqueue child calls for BFS
            frame_queue.extend(call_frame.calls);
        }

        debug!(
            target: "aa-rpc",
            tx_hash = %tx_hash,
            user_op_hash = %user_op_hash,
            "UserOperation not found in transaction trace"
        );
        Ok(None)
    }

    /// Internal gas estimation implementation using the GasEstimator
    ///
    /// This method performs gas estimation using eth_call simulation:
    /// - PreVerificationGas calculation (calldata costs + L1 data fee)
    /// - Verification gas limit estimation via simulateHandleOp
    /// - Call gas limit from simulation results
    ///
    /// Uses `RethPreVerificationGasCalculator` which leverages reth/revm's native L1 fee
    /// calculation with proper hardfork support (Bedrock, Ecotone, Fjord, Isthmus).
    ///
    /// # Arguments
    /// * `user_op` - The UserOperation to estimate gas for
    /// * `entry_point` - Address of the EntryPoint contract
    /// * `_version` - The EntryPoint version (used for logging, actual version determined by GasEstimator)
    /// * `user_state_override` - Optional user-provided state overrides (e.g., account code/balance)
    async fn estimate_gas_internal(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
        _version: EntryPointVersion,
        user_state_override: Option<StateOverride>,
    ) -> Result<UserOperationGasEstimate, GasEstimationError> {
        // Create simulation provider and gas estimator
        let sim_provider = Arc::new(EthApiSimulationProvider::new(self.eth_api.clone()));

        // Use RethPreVerificationGasCalculator for accurate L1 fee calculation
        // This uses reth/revm's native L1BlockInfo extraction and hardfork-aware fee computation
        let chain_spec = self.provider.chain_spec();
        let pvg_calculator = Arc::new(RethPreVerificationGasCalculator::new(
            self.provider.clone(),
            chain_spec,
        ));

        let config = GasEstimationConfig::default();
        let gas_estimator = GasEstimator::new(sim_provider, pvg_calculator, config);

        let sender = match user_op {
            UserOperation::V06(op) => op.sender,
            UserOperation::V07(op) => op.sender,
        };
        debug!(
            target: "aa-rpc",
            entry_point = %entry_point,
            sender = %sender,
            "Starting gas estimation via GasEstimator"
        );

        // Use the GasEstimator which handles all version-specific logic
        let result = gas_estimator
            .estimate_gas(user_op, entry_point, user_state_override)
            .await?;

        debug!(
            target: "aa-rpc",
            pre_verification_gas = %result.pre_verification_gas,
            verification_gas_limit = %result.verification_gas_limit,
            call_gas_limit = %result.call_gas_limit,
            "Gas estimation complete"
        );

        Ok(UserOperationGasEstimate {
            pre_verification_gas: result.pre_verification_gas,
            verification_gas_limit: result.verification_gas_limit,
            call_gas_limit: result.call_gas_limit,
            paymaster_verification_gas_limit: result.paymaster_verification_gas_limit,
            paymaster_post_op_gas_limit: result.paymaster_post_op_gas_limit,
        })
    }
}

#[async_trait]
impl<Provider, Eth> AccountAbstractionApiServer for AccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockNumReader
        + ReceiptProvider
        + BlockReader
        + Clone
        + Send
        + Sync
        + 'static,
    Eth: FullEthApi<NetworkTypes = Optimism> + EthApiTypes + TraceExt + Clone + Send + Sync + 'static,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
{
    async fn send_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<B256> {
        let sender = match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received sendUserOperation request (v0.6)"
                );
                op.sender
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received sendUserOperation request (v0.7+)"
                );
                op.sender
            }
        };

        // Validate the UserOperation first
        debug!(
            target: "aa-rpc",
            sender = %sender,
            entry_point = %entry_point,
            "Validating UserOperation before sending"
        );

        let validation_result = self
            .validate_internal(&user_operation, entry_point)
            .await
            .map_err(|e| {
                warn!(
                    target: "aa-rpc",
                    sender = %sender,
                    error = %e,
                    "UserOperation validation failed"
                );
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INVALID_PARAMS_CODE,
                    format!("UserOperation validation failed: {}", e),
                    None::<String>,
                )
            })?;

        // Check if validation passed
        if !validation_result.valid {
            let reason = if validation_result.return_info.sig_failed {
                "Signature validation failed".to_string()
            } else if !validation_result.violations.is_empty() {
                format!(
                    "ERC-7562 violations: {}",
                    validation_result
                        .violations
                        .iter()
                        .map(|v| format!("{}", v))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                "Validation failed".to_string()
            };

            warn!(
                target: "aa-rpc",
                sender = %sender,
                reason = %reason,
                "UserOperation rejected"
            );

            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::error::INVALID_PARAMS_CODE,
                format!("UserOperation rejected: {}", reason),
                None::<String>,
            ));
        }

        debug!(
            target: "aa-rpc",
            sender = %sender,
            "UserOperation validated successfully, sending to bundler"
        );

        // Handle based on send mode (Tips or Mempool)
        let user_op_hash = match &self.send_handler {
            UserOpSendHandler::Tips(tips_client) => {
                // Send to external TIPS service
                tips_client
                    .send_user_operation(user_operation, entry_point)
                    .await
                    .map_err(|e| {
                        jsonrpsee::types::ErrorObjectOwned::owned(
                            jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                            format!("Failed to send user operation to TIPS: {}", e),
                            None::<String>,
                        )
                    })?
            }
            UserOpSendHandler::Mempool {
                pool,
                chain_id,
                gossip_handle,
            } => {
                // Compute UserOp hash
                let user_op_hash = user_operation.hash(entry_point, *chain_id);

                // Add to local mempool
                {
                    let mut pool_guard = pool.write();
                    pool_guard
                        .add(user_operation.clone(), entry_point, validation_result)
                        .map_err(|e| {
                            warn!(
                                target: "aa-rpc",
                                sender = %sender,
                                error = %e,
                                "Failed to add UserOperation to mempool"
                            );
                            jsonrpsee::types::ErrorObjectOwned::owned(
                                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                                format!("Failed to add UserOperation to mempool: {}", e),
                                None::<String>,
                            )
                        })?;
                }

                debug!(
                    target: "aa-rpc",
                    sender = %sender,
                    hash = %user_op_hash,
                    "UserOperation added to mempool"
                );

                // Optionally gossip to peers
                if let Some(handle) = gossip_handle {
                    if let Err(e) = handle
                        .broadcast_user_op(user_operation, entry_point, user_op_hash, *chain_id)
                        .await
                    {
                        // Log but don't fail - local mempool succeeded
                        warn!(
                            target: "aa-rpc",
                            hash = %user_op_hash,
                            error = %e,
                            "Failed to gossip UserOperation to peers"
                        );
                    }
                }

                user_op_hash
            }
        };

        Ok(user_op_hash)
    }

    async fn estimate_user_operation_gas(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
        state_override: Option<StateOverride>,
    ) -> RpcResult<UserOperationGasEstimate> {
        // Validate entry point is supported
        let version = get_entrypoint_version(entry_point).ok_or_else(|| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::error::INVALID_PARAMS_CODE,
                format!("Unsupported EntryPoint: {}", entry_point),
                None::<String>,
            )
        })?;

        // Log the request
        let sender = match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    version = "v0.6",
                    has_state_override = state_override.is_some(),
                    "Received estimateUserOperationGas request"
                );
                op.sender
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    version = "v0.7+",
                    has_state_override = state_override.is_some(),
                    "Received estimateUserOperationGas request"
                );
                op.sender
            }
        };

        // Estimate gas using the estimation infrastructure
        let estimate = self
            .estimate_gas_internal(&user_operation, entry_point, version, state_override)
            .await
            .map_err(|e| {
                warn!(
                    target: "aa-rpc",
                    sender = %sender,
                    entry_point = %entry_point,
                    error = %e,
                    "Gas estimation failed"
                );
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    format!("Gas estimation failed: {}", e),
                    None::<String>,
                )
            })?;

        debug!(
            target: "aa-rpc",
            sender = %sender,
            pre_verification_gas = %estimate.pre_verification_gas,
            verification_gas_limit = %estimate.verification_gas_limit,
            call_gas_limit = %estimate.call_gas_limit,
            "Gas estimation complete"
        );

        Ok(estimate)
    }

    async fn get_user_operation_by_hash(
        &self,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationWithMetadata>> {
        debug!(
            hash = %user_operation_hash,
            "Received getUserOperationByHash request"
        );

        // Use the receipt provider to look up the UserOperation event (already included in a block)
        let lookup = match self.receipt_provider.lookup_user_operation(user_operation_hash).await {
            Ok(Some(lookup)) => lookup,
            Ok(None) => {
                // TODO: Implement pending/status check for UserOperations not yet included in a block.
                // Per EIP-4337, if the system is aware of the UserOp (e.g., in mempool/pending),
                // we should return partial data with status information.
                //
                // Options:
                // 1. Query TIPS service to check if UserOp is pending in the bundler mempool
                // 2. Maintain local pending UserOp cache and check there
                // 3. Return a status field indicating "pending" vs "not found"
                //
                // For now, we return None (not found) when not included in a block.
                debug!(
                    target: "aa-rpc",
                    hash = %user_operation_hash,
                    "UserOperation not found in blocks (pending status check not yet implemented)"
                );
                return Ok(None);
            }
            Err(e) => {
                warn!(
                    target: "aa-rpc",
                    hash = %user_operation_hash,
                    error = %e,
                    "Error looking up UserOperation"
                );
                return Ok(None);
            }
        };

        let indexed = &lookup.indexed;
        let tx_hash = indexed.transaction_hash;
        let entry_point = indexed.entry_point;

        // Get the chain ID for hash verification
        let chain_id = self.provider.chain_spec().chain().id();

        debug!(
            target: "aa-rpc",
            tx_hash = %tx_hash,
            entry_point = %entry_point,
            block_number = indexed.block_number,
            "Found UserOperation event, extracting full UserOp from transaction"
        );

        // First try to extract from direct EntryPoint call
        let user_operation = self.extract_user_operation_from_tx(
            user_operation_hash,
            tx_hash,
            entry_point,
            indexed.block_number,
            chain_id,
        )?;

        // If direct extraction succeeded, return the result
        if let Some(op) = user_operation {
            return Ok(Some(UserOperationWithMetadata {
                user_operation: op,
                entry_point: indexed.entry_point,
                block_number: indexed.block_number,
                block_hash: indexed.block_hash,
                transaction_hash: indexed.transaction_hash,
            }));
        }

        // Direct extraction failed - try tracing for subcall extraction
        debug!(
            target: "aa-rpc",
            hash = %user_operation_hash,
            tx_hash = %tx_hash,
            "Direct extraction failed, falling back to transaction trace"
        );

        let traced_operation = self
            .trace_find_user_operation(tx_hash, user_operation_hash, entry_point, chain_id)
            .await?;

        match traced_operation {
            Some(op) => Ok(Some(UserOperationWithMetadata {
                user_operation: op,
                entry_point: indexed.entry_point,
                block_number: indexed.block_number,
                block_hash: indexed.block_hash,
                transaction_hash: indexed.transaction_hash,
            })),
            None => {
                // Couldn't extract UserOp from either calldata or trace
                warn!(
                    target: "aa-rpc",
                    hash = %user_operation_hash,
                    tx_hash = %tx_hash,
                    "Could not extract UserOperation from transaction calldata or trace"
                );
                Err(jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    "UserOperation found in event but could not be extracted from transaction",
                    None::<String>,
                ))
            }
        }
    }

    async fn get_user_operation_receipt(
        &self,
        user_operation_hash: B256,
    ) -> RpcResult<Option<UserOperationReceipt>> {
        debug!(
            hash = %user_operation_hash,
            "Received getUserOperationReceipt request"
        );

        // Use the receipt provider to look up the UserOperation
        match self.receipt_provider.lookup_user_operation(user_operation_hash).await {
            Ok(Some(lookup)) => {
            debug!(
                target: "aa-rpc",
                hash = %user_operation_hash,
                    block_number = lookup.indexed.block_number,
                    "Found UserOperation, building receipt"
                );

                match self.build_receipt_from_lookup(lookup) {
                    Ok(receipt) => Ok(Some(receipt)),
                    Err(e) => {
                        warn!(
                            target: "aa-rpc",
                            hash = %user_operation_hash,
                            error = %e,
                            "Failed to build receipt"
                        );
                        Ok(None)
                    }
                }
            }
            Ok(None) => {
                debug!(
                    target: "aa-rpc",
                    hash = %user_operation_hash,
                    "UserOperation not found"
                );
                Ok(None)
            }
            Err(e) => {
                warn!(
                    target: "aa-rpc",
                    hash = %user_operation_hash,
                    error = %e,
                    "Error looking up UserOperation"
                );
                Ok(None)
            }
        }
    }

    async fn supported_entry_points(&self) -> RpcResult<Vec<Address>> {
        info!("Received supportedEntryPoints request");

        Ok(crate::entrypoint::supported_entrypoints())
    }
}

/// Implementation of the base account abstraction RPC API
pub struct BaseAccountAbstractionApiImpl<Provider, Eth> {
    #[allow(dead_code)]
    provider: Provider,
    eth_api: Eth,
}

impl<Provider, Eth> BaseAccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec> + Clone,
    Eth: Clone,
{
    /// Creates a new instance of BaseAccountAbstractionApi
    pub fn new(provider: Provider, eth_api: Eth) -> Self {
        Self { provider, eth_api }
    }
}

impl<Provider, Eth> BaseAccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + Clone
        + Send
        + Sync,
    Eth: FullEthApi<NetworkTypes = Optimism> + EthApiTypes + TraceExt + Clone + Send + Sync + 'static,
{
    /// Internal validation implementation
    async fn validate_internal(
        &self,
        user_operation: &UserOperation,
        entry_point: Address,
    ) -> Result<crate::simulation::ValidationOutput, crate::simulation::ValidationError> {
        use crate::simulation::UserOperationValidator;

        let sim_provider = Arc::new(EthApiSimulationProvider::new(self.eth_api.clone()));
        let validator = UserOperationValidator::new(sim_provider);

        validator.validate(user_operation, entry_point).await
    }
}

#[async_trait]
impl<Provider, Eth> BaseAccountAbstractionApiServer for BaseAccountAbstractionApiImpl<Provider, Eth>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + Clone
        + Send
        + Sync
        + 'static,
    Eth: FullEthApi<NetworkTypes = Optimism> + EthApiTypes + TraceExt + Clone + Send + Sync + 'static,
{
    async fn validate_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> RpcResult<ValidationResult> {
        let sender = match &user_operation {
            UserOperation::V06(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received validateUserOperation request (v0.6)"
                );
                op.sender
            }
            UserOperation::V07(op) => {
                info!(
                    sender = %op.sender,
                    entry_point = %entry_point,
                    "Received validateUserOperation request (v0.7+)"
                );
                op.sender
            }
        };

        // Run validation
        match self.validate_internal(&user_operation, entry_point).await {
            Ok(output) => {
                debug!(
                    target: "aa-rpc",
                    sender = %sender,
                    valid = output.valid,
                    sig_failed = output.return_info.sig_failed,
                    valid_after = output.return_info.valid_after,
                    valid_until = output.return_info.valid_until,
                    violations = output.violations.len(),
                    "Validation complete"
                );

                // Build context with entity stake info
                let context = ValidationContext {
                    sender_info: EntityStakeInfo {
                        address: output.sender_info.address,
                        stake: output.sender_info.stake,
                        unstake_delay_sec: output.sender_info.unstake_delay_sec,
                        deposit: output.sender_info.deposit,
                        is_staked: output.sender_info.is_staked,
                    },
                    factory_info: output.factory_info.map(|f| EntityStakeInfo {
                        address: f.address,
                        stake: f.stake,
                        unstake_delay_sec: f.unstake_delay_sec,
                        deposit: f.deposit,
                        is_staked: f.is_staked,
                    }),
                    paymaster_info: output.paymaster_info.map(|p| EntityStakeInfo {
                        address: p.address,
                        stake: p.stake,
                        unstake_delay_sec: p.unstake_delay_sec,
                        deposit: p.deposit,
                        is_staked: p.is_staked,
                    }),
                    aggregator_info: output.aggregator_info.map(|a| AggregatorInfo {
                        aggregator: a.aggregator,
                        stake_info: EntityStakeInfo {
                            address: a.stake_info.address,
                            stake: a.stake_info.stake,
                            unstake_delay_sec: a.stake_info.unstake_delay_sec,
                            deposit: a.stake_info.deposit,
                            is_staked: a.stake_info.is_staked,
                        },
                    }),
                    code_hashes: output.code_hashes.map(|ch| CodeHashes {
                        sender: ch.sender,
                        factory: ch.factory,
                        paymaster: ch.paymaster,
                        aggregator: ch.aggregator,
                    }),
                };

                // Build reason if invalid
                let reason = if !output.valid {
                    if output.return_info.sig_failed {
                        Some("Signature validation failed".to_string())
                    } else if !output.violations.is_empty() {
                        Some(format!(
                            "ERC-7562 violations: {}",
                            output
                                .violations
                                .iter()
                                .map(|v| format!("{}", v))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok(ValidationResult {
                    valid: output.valid,
                    reason,
                    valid_until: Some(output.return_info.valid_until),
                    valid_after: Some(output.return_info.valid_after),
                    context: Some(context),
                })
            }
            Err(e) => {
                warn!(
                    target: "aa-rpc",
                    sender = %sender,
                    error = %e,
                    "Validation failed"
                );

                Ok(ValidationResult {
                    valid: false,
                    reason: Some(e.to_string()),
                    valid_until: None,
                    valid_after: None,
                    context: None,
                })
            }
        }
    }
}
