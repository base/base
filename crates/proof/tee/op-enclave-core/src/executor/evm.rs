//! EVM execution wrapper for stateless block execution.
//!
//! This module provides a wrapper around the kona-executor's `StatelessL2Builder`
//! for executing L2 blocks in a stateless manner within an enclave.

use alloy_consensus::{Header, Sealed};
use alloy_evm::precompiles::PrecompilesMap;
use alloy_evm::{Database, EvmEnv, EvmFactory};
use alloy_op_evm::OpEvm;
use alloy_primitives::{Address, B64, B256, Bytes, U256};
use alloy_rpc_types_engine::PayloadAttributes;
use kona_executor::StatelessL2Builder;
use kona_genesis::RollupConfig;
use kona_mpt::TrieHinter;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use op_revm::precompiles::OpPrecompiles;
// Re-export L1BlockInfo for use by callers
pub use op_revm::L1BlockInfo;
use op_revm::{
    DefaultOp, OpBuilder, OpContext, OpHaltReason, OpSpecId, OpTransaction, OpTransactionError,
};
use revm::Inspector;
use revm::context::{BlockEnv, TxEnv};
use revm::context_interface::result::EVMError;
use revm::inspector::NoOpInspector;

use super::trie_db::EnclaveTrieDB;
use crate::error::ExecutorError;

/// Custom EVM factory for enclave execution that pre-populates L1BlockInfo.
///
/// This factory wraps the standard OpEvmFactory but initializes the EVM with
/// pre-computed L1BlockInfo values from the L1 origin header and system config.
/// This is necessary for stateless execution where the L1Block contract storage
/// may contain stale values from the previous block.
#[derive(Debug, Clone)]
pub struct EnclaveEvmFactory {
    /// Pre-computed L1BlockInfo with values from the L1 origin header
    l1_block_info: L1BlockInfo,
}

impl EnclaveEvmFactory {
    /// Creates a new enclave EVM factory with the given L1BlockInfo.
    #[must_use]
    pub const fn new(l1_block_info: L1BlockInfo) -> Self {
        Self { l1_block_info }
    }
}

impl EvmFactory for EnclaveEvmFactory {
    type Evm<DB: Database, I: Inspector<OpContext<DB>>> = OpEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = OpContext<DB>;
    type Tx = OpTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        // Create a mutable copy of l1_block_info and set the l2_block to the current block number
        let mut l1_block_info = self.l1_block_info.clone();
        l1_block_info.l2_block = Some(input.block_env.number);

        OpEvm::new(
            revm::Context::op()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_chain(l1_block_info)
                .build_op_with_inspector(NoOpInspector {})
                .with_precompiles(PrecompilesMap::from_static(
                    OpPrecompiles::new_with_spec(spec_id).precompiles(),
                )),
            false,
        )
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let mut l1_block_info = self.l1_block_info.clone();
        l1_block_info.l2_block = Some(input.block_env.number);

        OpEvm::new(
            revm::Context::op()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_chain(l1_block_info)
                .build_op_with_inspector(inspector)
                .with_precompiles(PrecompilesMap::from_static(
                    OpPrecompiles::new_with_spec(spec_id).precompiles(),
                )),
            true,
        )
    }
}

/// Build L1BlockInfo from the previous block's L1 info deposit transaction.
///
/// This extracts the L1BlockInfo values that were actually used for the previous block,
/// which should match what we need for the current block (assuming same L1 origin).
/// If the L1 origin changed, the values will be updated by the new L1 info deposit.
///
/// # Arguments
///
/// * `prev_deposit_calldata` - The calldata from the previous block's L1 info deposit
/// * `spec_id` - The OP spec ID for the current block
///
/// # Returns
///
/// An L1BlockInfo populated with the values from the previous block.
///
/// # Errors
///
/// Returns an error if the calldata cannot be decoded.
pub fn build_l1_block_info_from_deposit(
    prev_deposit_calldata: &Bytes,
    spec_id: OpSpecId,
) -> Result<L1BlockInfo, String> {
    use kona_protocol::L1BlockInfoTx;

    // Decode the L1BlockInfoTx from the calldata
    let l1_info_tx = L1BlockInfoTx::decode_calldata(prev_deposit_calldata)
        .map_err(|e| format!("Failed to decode L1BlockInfoTx: {e}"))?;

    // Extract values from the L1BlockInfoTx
    let l1_base_fee = l1_info_tx.l1_base_fee();
    let l1_blob_base_fee = if l1_info_tx.blob_base_fee() != U256::ZERO {
        Some(l1_info_tx.blob_base_fee())
    } else {
        None
    };
    let l1_base_fee_scalar = l1_info_tx.l1_fee_scalar();
    let l1_blob_base_fee_scalar = if l1_info_tx.blob_base_fee_scalar() != U256::ZERO {
        Some(l1_info_tx.blob_base_fee_scalar())
    } else {
        None
    };
    let l1_fee_overhead = if l1_info_tx.l1_fee_overhead() != U256::ZERO {
        Some(l1_info_tx.l1_fee_overhead())
    } else {
        None
    };

    // For Isthmus+, extract operator fee parameters
    let (operator_fee_scalar, operator_fee_constant) = if spec_id.is_enabled_in(OpSpecId::ISTHMUS) {
        (
            Some(U256::from(l1_info_tx.operator_fee_scalar())),
            Some(U256::from(l1_info_tx.operator_fee_constant())),
        )
    } else {
        (None, None)
    };

    Ok(L1BlockInfo {
        l2_block: None, // Will be set by the EVM factory
        l1_base_fee,
        l1_fee_overhead,
        l1_base_fee_scalar,
        l1_blob_base_fee,
        l1_blob_base_fee_scalar,
        operator_fee_scalar,
        operator_fee_constant,
        da_footprint_gas_scalar: l1_info_tx.da_footprint(),
        empty_ecotone_scalars: l1_info_tx.empty_scalars(),
        tx_l1_cost: None,
    })
}

/// Result of EVM block execution.
#[derive(Debug, Clone)]
pub struct BlockExecutionResult {
    /// The computed state root after execution.
    pub state_root: B256,

    /// The computed receipts root after execution.
    pub receipts_root: B256,

    /// Gas used by the block.
    pub gas_used: u64,
}

/// No-op trie hinter for enclave execution.
///
/// In the enclave context, we don't need to hint for state access
/// since all required state is provided in the witness.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnclaveTrieHinter;

impl TrieHinter for EnclaveTrieHinter {
    type Error = String;

    fn hint_trie_node(&self, _hash: B256) -> Result<(), Self::Error> {
        // No-op: all state is pre-loaded from witness
        Ok(())
    }

    fn hint_account_proof(&self, _address: Address, _block_number: u64) -> Result<(), Self::Error> {
        // No-op: all state is pre-loaded from witness
        Ok(())
    }

    fn hint_storage_proof(
        &self,
        _address: Address,
        _slot: U256,
        _block_number: u64,
    ) -> Result<(), Self::Error> {
        // No-op: all state is pre-loaded from witness
        Ok(())
    }

    fn hint_execution_witness(
        &self,
        _parent_hash: B256,
        _op_payload_attributes: &OpPayloadAttributes,
    ) -> Result<(), Self::Error> {
        // No-op: witness is already provided
        Ok(())
    }
}

/// Execute a block using the stateless L2 executor.
///
/// This executes the given transactions against the parent state provided
/// by the `EnclaveTrieDB` and returns the computed state root and receipts root.
///
/// # Arguments
///
/// * `rollup_config` - The rollup configuration
/// * `parent_header` - The parent block header (sealed, takes ownership)
/// * `block_header` - The block header being executed (for payload attributes)
/// * `transactions` - The transactions to execute (EIP-2718 encoded, deposits first)
/// * `trie_db` - The trie database with pre-loaded state (consumed by builder)
/// * `l1_block_info` - Pre-computed L1 block info for L1 data fee calculation
///
/// # Returns
///
/// The execution result containing computed roots.
///
/// # Errors
///
/// Returns an error if block execution fails.
pub fn execute_block(
    rollup_config: &RollupConfig,
    parent_header: Sealed<Header>,
    block_header: &Header,
    transactions: &[Bytes],
    trie_db: EnclaveTrieDB,
    l1_block_info: L1BlockInfo,
) -> Result<BlockExecutionResult, ExecutorError> {
    // Extract EIP-1559 params from extra_data for Holocene+
    // After Holocene, blocks must have the EIP-1559 parameters in extra_data[1..9]
    let eip_1559_params = if rollup_config.is_holocene_active(block_header.timestamp) {
        block_header
            .extra_data
            .get(1..9)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .map(B64::from)
    } else {
        None
    };

    // Extract min_base_fee from extra_data for Jovian+
    // After Jovian, blocks must have the min_base_fee in extra_data[9..17]
    let min_base_fee = if rollup_config.is_jovian_active(block_header.timestamp) {
        block_header
            .extra_data
            .get(9..17)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .map(u64::from_be_bytes)
    } else {
        None
    };

    // Build payload attributes from block header
    let attrs = OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: block_header.timestamp,
            prev_randao: block_header.mix_hash,
            suggested_fee_recipient: block_header.beneficiary,
            withdrawals: None,
            parent_beacon_block_root: block_header.parent_beacon_block_root,
        },
        transactions: Some(transactions.to_vec()),
        no_tx_pool: Some(true),
        gas_limit: Some(block_header.gas_limit),
        eip_1559_params,
        min_base_fee,
    };

    // Create custom EVM factory with pre-populated L1BlockInfo
    let evm_factory = EnclaveEvmFactory::new(l1_block_info);

    // Create stateless L2 builder
    let mut builder = StatelessL2Builder::new(
        rollup_config,
        evm_factory,
        trie_db,
        EnclaveTrieHinter,
        parent_header,
    );

    // Execute the block
    let outcome = builder
        .build_block(attrs)
        .map_err(|e| ExecutorError::ExecutionFailed(format!("{e}")))?;

    Ok(BlockExecutionResult {
        state_root: outcome.header.state_root,
        receipts_root: outcome.header.receipts_root,
        gas_used: outcome.execution_result.gas_used,
    })
}

/// Verify that a block's execution results match the expected values.
///
/// This compares the computed roots from EVM execution against the
/// values in the block header.
///
/// # Arguments
///
/// * `expected_state_root` - The state root from the block header
/// * `expected_receipts_root` - The receipts root from the block header
/// * `actual` - The actual execution result
///
/// # Errors
///
/// Returns an error if either root doesn't match.
pub fn verify_execution_result(
    expected_state_root: B256,
    expected_receipts_root: B256,
    actual: &BlockExecutionResult,
) -> Result<(), ExecutorError> {
    if actual.state_root != expected_state_root {
        return Err(ExecutorError::InvalidStateRoot {
            expected: expected_state_root,
            computed: actual.state_root,
        });
    }

    if actual.receipts_root != expected_receipts_root {
        return Err(ExecutorError::InvalidReceiptHash {
            expected: expected_receipts_root,
            computed: actual.receipts_root,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enclave_trie_hinter() {
        let hinter = EnclaveTrieHinter;

        // All hint methods should succeed (no-op)
        assert!(hinter.hint_trie_node(B256::ZERO).is_ok());
        assert!(hinter.hint_account_proof(Address::ZERO, 0).is_ok());
        assert!(
            hinter
                .hint_storage_proof(Address::ZERO, U256::ZERO, 0)
                .is_ok()
        );
    }

    #[test]
    fn test_verify_execution_result_success() {
        let state_root = B256::repeat_byte(0xAA);
        let receipts_root = B256::repeat_byte(0xBB);

        let result = BlockExecutionResult {
            state_root,
            receipts_root,
            gas_used: 21000,
        };

        assert!(verify_execution_result(state_root, receipts_root, &result).is_ok());
    }

    #[test]
    fn test_verify_execution_result_state_mismatch() {
        let expected_state_root = B256::repeat_byte(0xAA);
        let actual_state_root = B256::repeat_byte(0xCC);
        let receipts_root = B256::repeat_byte(0xBB);

        let result = BlockExecutionResult {
            state_root: actual_state_root,
            receipts_root,
            gas_used: 21000,
        };

        let err = verify_execution_result(expected_state_root, receipts_root, &result);
        assert!(matches!(err, Err(ExecutorError::InvalidStateRoot { .. })));
    }

    #[test]
    fn test_verify_execution_result_receipts_mismatch() {
        let state_root = B256::repeat_byte(0xAA);
        let expected_receipts_root = B256::repeat_byte(0xBB);
        let actual_receipts_root = B256::repeat_byte(0xDD);

        let result = BlockExecutionResult {
            state_root,
            receipts_root: actual_receipts_root,
            gas_used: 21000,
        };

        let err = verify_execution_result(state_root, expected_receipts_root, &result);
        assert!(matches!(err, Err(ExecutorError::InvalidReceiptHash { .. })));
    }
}
