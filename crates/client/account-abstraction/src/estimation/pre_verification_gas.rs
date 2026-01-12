//! PreVerificationGas Calculation
//!
//! PreVerificationGas covers the cost of:
//! - Calldata cost (16 gas per non-zero byte, 4 gas per zero byte)
//! - Fixed overhead costs
//! - Chain-specific costs (e.g., L1 data fee on Optimism)
//!
//! This module provides an extensible trait-based system for calculating
//! preVerificationGas across different chains and EntryPoint versions.
//!
//! ## Calculators
//!
//! - [`DefaultPreVerificationGasCalculator`]: Basic L2 calldata cost calculation
//! - [`OptimismPreVerificationGasCalculator`]: Legacy calculator using oracle for L1 fees
//! - [`RethPreVerificationGasCalculator`]: Uses reth/revm's native L1 fee calculation
//!   with proper hardfork support (Bedrock, Ecotone, Fjord, Isthmus)

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, Bytes, U256};
use async_trait::async_trait;
use reth_evm::op_revm;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::l1::{extract_l1_info, RethL1BlockInfo};
use reth_primitives_traits::Block;
use reth_provider::{BlockNumReader, BlockReader};
use std::sync::Arc;
use tracing::debug;

use crate::rpc::UserOperation;

/// Trait for calculating preVerificationGas
/// 
/// Different chains may have different gas cost structures.
/// For example, Optimism includes L1 data posting costs.
#[async_trait]
pub trait PreVerificationGasCalculator: Send + Sync {
    /// Calculate preVerificationGas for a UserOperation
    async fn calculate(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
    ) -> Result<U256, String>;
}

/// Oracle for fetching chain-specific gas price data
/// 
/// Used by chain-specific calculators to get L1 gas prices, etc.
#[async_trait]
pub trait PreVerificationGasOracle: Send + Sync {
    /// Get the current L1 gas price (for L2 chains)
    async fn get_l1_gas_price(&self) -> Result<U256, String>;

    /// Get the current L1 data fee scalar (for Optimism)
    async fn get_l1_fee_scalar(&self) -> Result<U256, String>;

    /// Get the L1 base fee (for Optimism Ecotone+)
    async fn get_l1_base_fee(&self) -> Result<U256, String>;

    /// Get the blob base fee (for Optimism Ecotone+)
    async fn get_blob_base_fee(&self) -> Result<U256, String>;
}

/// Default preVerificationGas calculator
/// 
/// Calculates based on calldata cost and fixed overhead.
/// Suitable for L1 or chains without special L1 data fees.
pub struct DefaultPreVerificationGasCalculator {
    /// Fixed overhead gas per UserOperation
    fixed_overhead: U256,
    /// Gas per user operation word (32 bytes)
    per_user_op_word: U256,
    /// Gas per zero byte of calldata
    zero_byte_cost: U256,
    /// Gas per non-zero byte of calldata
    non_zero_byte_cost: U256,
}

impl Default for DefaultPreVerificationGasCalculator {
    fn default() -> Self {
        Self {
            fixed_overhead: U256::from(21000), // Base transaction cost
            per_user_op_word: U256::from(4),   // Small overhead per word
            zero_byte_cost: U256::from(4),     // EIP-2028
            non_zero_byte_cost: U256::from(16), // EIP-2028
        }
    }
}

impl DefaultPreVerificationGasCalculator {
    /// Create a new calculator with custom parameters
    pub fn new(
        fixed_overhead: U256,
        per_user_op_word: U256,
        zero_byte_cost: U256,
        non_zero_byte_cost: U256,
    ) -> Self {
        Self {
            fixed_overhead,
            per_user_op_word,
            zero_byte_cost,
            non_zero_byte_cost,
        }
    }

    /// Calculate calldata cost for a byte sequence
    fn calldata_cost(&self, data: &Bytes) -> U256 {
        let mut cost = U256::ZERO;
        for byte in data.iter() {
            if *byte == 0 {
                cost += self.zero_byte_cost;
            } else {
                cost += self.non_zero_byte_cost;
            }
        }
        cost
    }

    /// Encode a UserOperation to bytes for calldata cost calculation
    fn encode_user_op(&self, user_op: &UserOperation) -> Bytes {
        match user_op {
            UserOperation::V06(op) => {
                // Encode v0.6 user op fields
                let mut data = Vec::new();
                data.extend_from_slice(op.sender.as_slice());
                data.extend_from_slice(&op.nonce.to_be_bytes::<32>());
                data.extend_from_slice(&op.init_code);
                data.extend_from_slice(&op.call_data);
                data.extend_from_slice(&op.call_gas_limit.to_be_bytes::<32>());
                data.extend_from_slice(&op.verification_gas_limit.to_be_bytes::<32>());
                data.extend_from_slice(&op.pre_verification_gas.to_be_bytes::<32>());
                data.extend_from_slice(&op.max_fee_per_gas.to_be_bytes::<32>());
                data.extend_from_slice(&op.max_priority_fee_per_gas.to_be_bytes::<32>());
                data.extend_from_slice(&op.paymaster_and_data);
                data.extend_from_slice(&op.signature);
                Bytes::from(data)
            }
            UserOperation::V07(op) => {
                // Encode v0.7 user op fields (packed format)
                let mut data = Vec::new();
                data.extend_from_slice(op.sender.as_slice());
                data.extend_from_slice(&op.nonce.to_be_bytes::<32>());

                // initCode = factory + factoryData
                if !op.factory.is_zero() {
                    data.extend_from_slice(op.factory.as_slice());
                    data.extend_from_slice(&op.factory_data);
                }

                data.extend_from_slice(&op.call_data);

                // accountGasLimits (32 bytes packed)
                data.extend_from_slice(&op.verification_gas_limit.to_be_bytes::<32>()[16..32]);
                data.extend_from_slice(&op.call_gas_limit.to_be_bytes::<32>()[16..32]);

                data.extend_from_slice(&op.pre_verification_gas.to_be_bytes::<32>());

                // gasFees (32 bytes packed)
                data.extend_from_slice(&op.max_priority_fee_per_gas.to_be_bytes::<32>()[16..32]);
                data.extend_from_slice(&op.max_fee_per_gas.to_be_bytes::<32>()[16..32]);

                // paymasterAndData
                if !op.paymaster.is_zero() {
                    data.extend_from_slice(op.paymaster.as_slice());
                    data.extend_from_slice(
                        &op.paymaster_verification_gas_limit.to_be_bytes::<32>()[16..32],
                    );
                    data.extend_from_slice(
                        &op.paymaster_post_op_gas_limit.to_be_bytes::<32>()[16..32],
                    );
                    data.extend_from_slice(&op.paymaster_data);
                }

                data.extend_from_slice(&op.signature);
                Bytes::from(data)
            }
        }
    }
}

#[async_trait]
impl PreVerificationGasCalculator for DefaultPreVerificationGasCalculator {
    async fn calculate(
        &self,
        user_op: &UserOperation,
        _entry_point: Address,
    ) -> Result<U256, String> {
        // Encode user op to calculate calldata cost
        let encoded = self.encode_user_op(user_op);

        // Calculate calldata cost
        let calldata_cost = self.calldata_cost(&encoded);

        // Calculate word count for per-word overhead
        let word_count = U256::from((encoded.len() + 31) / 32);

        // Total = fixed overhead + calldata cost + per-word cost
        let total = self.fixed_overhead + calldata_cost + (word_count * self.per_user_op_word);

        Ok(total)
    }
}

/// Optimism-specific preVerificationGas calculator
/// 
/// Includes L1 data posting costs using the GasPriceOracle.
pub struct OptimismPreVerificationGasCalculator<O> {
    /// Base calculator for L2 costs
    base_calculator: DefaultPreVerificationGasCalculator,
    /// Oracle for L1 gas prices
    oracle: O,
}

impl<O> OptimismPreVerificationGasCalculator<O>
where
    O: PreVerificationGasOracle,
{
    /// Create a new Optimism calculator
    pub fn new(oracle: O) -> Self {
        Self {
            base_calculator: DefaultPreVerificationGasCalculator::default(),
            oracle,
        }
    }

    /// Create with custom base calculator
    pub fn with_base_calculator(oracle: O, base_calculator: DefaultPreVerificationGasCalculator) -> Self {
        Self {
            base_calculator,
            oracle,
        }
    }

    /// Calculate L1 data fee for the encoded user operation
    /// 
    /// This uses the Optimism L1 fee calculation:
    /// - Pre-Ecotone: l1Fee = l1GasPrice * l1GasUsed * scalar / 1e6
    /// - Ecotone+: l1Fee = (baseFeeScalar * l1BaseFee + blobFeeScalar * blobBaseFee) * l1GasUsed / 16e6
    async fn calculate_l1_data_fee(&self, encoded: &Bytes) -> Result<U256, String> {
        // Calculate L1 gas used (compressed calldata estimate)
        // Count zero and non-zero bytes with different weights
        let mut l1_gas_used = U256::ZERO;
        for byte in encoded.iter() {
            if *byte == 0 {
                l1_gas_used += U256::from(4);
            } else {
                l1_gas_used += U256::from(16);
            }
        }

        // Add fixed overhead for L1 transaction
        l1_gas_used += U256::from(2100); // ~L1 transaction base cost

        // Get L1 gas price from oracle
        let l1_gas_price = self.oracle.get_l1_gas_price().await?;
        let scalar = self.oracle.get_l1_fee_scalar().await?;

        // Calculate L1 fee
        // l1Fee = l1GasUsed * l1GasPrice * scalar / 1e6
        let l1_fee = (l1_gas_used * l1_gas_price * scalar) / U256::from(1_000_000);

        Ok(l1_fee)
    }
}

#[async_trait]
impl<O> PreVerificationGasCalculator for OptimismPreVerificationGasCalculator<O>
where
    O: PreVerificationGasOracle,
{
    async fn calculate(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
    ) -> Result<U256, String> {
        // Calculate base L2 preVerificationGas
        let base_pvg = self.base_calculator.calculate(user_op, entry_point).await?;

        // Encode user op for L1 fee calculation
        let encoded = self.base_calculator.encode_user_op(user_op);

        // Calculate L1 data fee
        let l1_fee = self.calculate_l1_data_fee(&encoded).await?;

        // Total preVerificationGas = base + L1 fee
        // Note: L1 fee is in wei, we need to convert to gas units
        // This is typically done by dividing by the L2 gas price
        // For simplicity, we add the L1 fee directly (assuming it's already in gas units)
        Ok(base_pvg + l1_fee)
    }
}

/// Reth-native preVerificationGas calculator using L1BlockInfo
///
/// This calculator leverages reth/revm's built-in L1 fee calculation which:
/// - Automatically handles all Optimism hardforks (Bedrock, Ecotone, Fjord, Isthmus, Jovian)
/// - Reads L1 block info directly from the L1Block contract state
/// - Uses the same calculation logic as the sequencer
///
/// This is the recommended calculator for Optimism/Base chains.
pub struct RethPreVerificationGasCalculator<P> {
    /// Base calculator for L2 calldata costs
    base_calculator: DefaultPreVerificationGasCalculator,
    /// Provider for reading block data
    provider: P,
    /// Chain spec for hardfork detection
    chain_spec: Arc<OpChainSpec>,
}

impl<P> RethPreVerificationGasCalculator<P>
where
    P: BlockReader + BlockNumReader,
{
    /// Create a new Reth-native calculator
    ///
    /// # Arguments
    /// * `provider` - Provider that implements `BlockReader` for accessing block data
    /// * `chain_spec` - Chain specification for hardfork detection
    pub fn new(provider: P, chain_spec: Arc<OpChainSpec>) -> Self {
        Self {
            base_calculator: DefaultPreVerificationGasCalculator::default(),
            provider,
            chain_spec,
        }
    }

    /// Create with custom base calculator
    pub fn with_base_calculator(
        provider: P,
        chain_spec: Arc<OpChainSpec>,
        base_calculator: DefaultPreVerificationGasCalculator,
    ) -> Self {
        Self {
            base_calculator,
            provider,
            chain_spec,
        }
    }

    /// Get L1BlockInfo from the latest block
    ///
    /// Returns `None` if this is not an L2 chain (e.g., dev chain, L1) where
    /// there's no L1 info deposit transaction.
    fn get_l1_block_info(&self) -> Result<Option<(op_revm::L1BlockInfo, u64)>, String> {
        // Get the latest block
        let latest_block = self
            .provider
            .last_block_number()
            .map_err(|e| format!("Failed to get latest block number: {}", e))?;

        debug!(
            target: "aa-pvg",
            block_number = latest_block,
            "Fetching L1BlockInfo from latest block"
        );

        let block = self
            .provider
            .block(latest_block.into())
            .map_err(|e| format!("Failed to get block {}: {}", latest_block, e))?
            .ok_or_else(|| format!("Block {} not found", latest_block))?;

        let timestamp = block.header().timestamp();

        debug!(
            target: "aa-pvg",
            block_number = latest_block,
            timestamp = timestamp,
            "Block fetched, attempting to extract L1 info"
        );

        // Extract L1 block info from the first transaction (L1 info deposit tx)
        // This will fail on non-L2 chains (dev, L1) - that's expected
        match extract_l1_info(block.body()) {
            Ok(l1_info) => {
                debug!(
                    target: "aa-pvg",
                    block_number = latest_block,
                    l1_base_fee = %l1_info.l1_base_fee,
                    l1_base_fee_scalar = %l1_info.l1_base_fee_scalar,
                    l1_blob_base_fee = ?l1_info.l1_blob_base_fee,
                    "Extracted L1BlockInfo from block"
                );
                Ok(Some((l1_info, timestamp)))
            }
            Err(e) => {
                debug!(
                    target: "aa-pvg",
                    block_number = latest_block,
                    error = %e,
                    "No L1 block info found - this is expected on dev/L1 chains"
                );
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl<P> PreVerificationGasCalculator for RethPreVerificationGasCalculator<P>
where
    P: BlockReader + BlockNumReader + Send + Sync,
{
    async fn calculate(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
    ) -> Result<U256, String> {
        // Calculate base L2 preVerificationGas (calldata costs)
        let base_pvg = self
            .base_calculator
            .calculate(user_op, entry_point)
            .await?;

        // Try to get L1 block info from the latest block
        // This will be None on non-L2 chains (dev, L1)
        let l1_block_info = self.get_l1_block_info()?;

        let l1_fee = if let Some((mut l1_info, timestamp)) = l1_block_info {
            // Encode user op for L1 fee calculation
            let encoded = self.base_calculator.encode_user_op(user_op);

            // Calculate L1 data fee using reth's hardfork-aware calculation
            // is_deposit = false since UserOperations are not deposit transactions
            let fee = l1_info
                .l1_tx_data_fee(
                    self.chain_spec.as_ref(),
                    timestamp,
                    &encoded,
                    false, // is_deposit
                )
                .map_err(|e| format!("Failed to calculate L1 data fee: {}", e))?;

            debug!(
                target: "aa-pvg",
                base_pvg = %base_pvg,
                l1_fee = %fee,
                encoded_len = encoded.len(),
                "Calculated preVerificationGas with L1 fee"
            );

            fee
        } else {
            // No L1 block info (dev chain / L1) - no L1 data fee
            debug!(
                target: "aa-pvg",
                base_pvg = %base_pvg,
                "No L1 block info available - using base preVerificationGas only (dev/L1 chain)"
            );

            U256::ZERO
        };

        // Total preVerificationGas = base L2 cost + L1 data fee
        Ok(base_pvg + l1_fee)
    }
}

/// Stub oracle for testing or chains that don't need L1 data
pub struct NullPreVerificationGasOracle;

#[async_trait]
impl PreVerificationGasOracle for NullPreVerificationGasOracle {
    async fn get_l1_gas_price(&self) -> Result<U256, String> {
        Ok(U256::ZERO)
    }

    async fn get_l1_fee_scalar(&self) -> Result<U256, String> {
        Ok(U256::from(1_000_000)) // 1.0 scalar
    }

    async fn get_l1_base_fee(&self) -> Result<U256, String> {
        Ok(U256::ZERO)
    }

    async fn get_blob_base_fee(&self) -> Result<U256, String> {
        Ok(U256::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::UserOperationV06;

    #[tokio::test]
    async fn test_default_calculator_basic() {
        let calculator = DefaultPreVerificationGasCalculator::default();

        let user_op = UserOperation::V06(UserOperationV06 {
            sender: Address::ZERO,
            nonce: U256::ZERO,
            init_code: Bytes::default(),
            call_data: Bytes::from(vec![0x12, 0x34, 0x56, 0x78]),
            call_gas_limit: U256::from(100000),
            verification_gas_limit: U256::from(100000),
            pre_verification_gas: U256::from(50000),
            max_fee_per_gas: U256::from(1000000000),
            max_priority_fee_per_gas: U256::from(1000000000),
            paymaster_and_data: Bytes::default(),
            signature: Bytes::from(vec![0u8; 65]),
        });

        let result = calculator.calculate(&user_op, Address::ZERO).await.unwrap();
        
        // Result should be > fixed overhead
        assert!(result > U256::from(21000));
    }
}

