//! L2 block reference parsing for L1 origin validation.
//!
//! This module provides functionality to parse `L2BlockInfo` from block headers
//! and first transaction data, matching the Go `derive.L2BlockToBlockRef()` function.

use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_eips::Typed2718;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{B256, Bytes};
use kona_genesis::RollupConfig;
use kona_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use op_alloy_consensus::OpTxEnvelope;

use crate::error::ExecutorError;

/// Parse `L2BlockInfo` from a block's header and first transaction.
///
/// This matches Go's `derive.L2BlockToBlockRef()` function which extracts
/// the L1 origin information from an L2 block.
///
/// # Arguments
///
/// * `rollup_config` - The rollup configuration
/// * `header` - The L2 block header
/// * `block_hash` - The L2 block hash (pre-computed for efficiency)
/// * `first_tx` - The first transaction bytes (L1 info deposit)
///
/// # Returns
///
/// The parsed `L2BlockInfo` containing block info and L1 origin.
///
/// # Errors
///
/// Returns an error if:
/// - The genesis block hash doesn't match
/// - The first transaction is not a deposit
/// - The `L1BlockInfo` cannot be decoded
pub fn l2_block_to_block_info(
    rollup_config: &RollupConfig,
    header: &Header,
    block_hash: B256,
    first_tx: &Bytes,
) -> Result<L2BlockInfo, ExecutorError> {
    // Build the L2 block info
    let block_info = BlockInfo::new(
        block_hash,
        header.number,
        header.parent_hash,
        header.timestamp,
    );

    // Handle genesis block
    if header.number == rollup_config.genesis.l2.number {
        // Verify genesis hash matches
        if block_hash != rollup_config.genesis.l2.hash {
            return Err(ExecutorError::ExecutionFailed(format!(
                "genesis hash mismatch: expected {}, got {}",
                rollup_config.genesis.l2.hash, block_hash
            )));
        }

        return Ok(L2BlockInfo::new(
            block_info,
            BlockNumHash {
                hash: rollup_config.genesis.l1.hash,
                number: rollup_config.genesis.l1.number,
            },
            0, // sequence_number is 0 for genesis
        ));
    }

    // Non-genesis block: decode the deposit tx and parse L1BlockInfo
    let tx = OpTxEnvelope::decode_2718(&mut first_tx.as_ref())
        .map_err(|e| ExecutorError::TxDecodeFailed(e.to_string()))?;

    let deposit = match &tx {
        OpTxEnvelope::Deposit(d) => d,
        _ => {
            return Err(ExecutorError::ExecutionFailed(format!(
                "first transaction is not a deposit, type: {}",
                tx.ty()
            )));
        }
    };

    // Parse L1BlockInfo from the deposit tx input
    let l1_info = L1BlockInfoTx::decode_calldata(deposit.input.as_ref()).map_err(|e| {
        ExecutorError::ExecutionFailed(format!("failed to decode L1BlockInfoTx: {e}"))
    })?;

    Ok(L2BlockInfo::new(
        block_info,
        l1_info.id(),
        l1_info.sequence_number(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_rollup_config;
    use alloy_primitives::{address, b256};

    fn test_header(number: u64, timestamp: u64) -> Header {
        Header {
            parent_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
            beneficiary: address!("0000000000000000000000000000000000000000"),
            state_root: b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            transactions_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            receipts_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            logs_bloom: Default::default(),
            difficulty: Default::default(),
            number,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp,
            extra_data: Default::default(),
            mix_hash: Default::default(),
            nonce: Default::default(),
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
        }
    }

    #[test]
    fn test_genesis_block() {
        let mut config = default_rollup_config();
        let genesis_hash =
            b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = genesis_hash;
        config.genesis.l1.number = 100;
        config.genesis.l1.hash =
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let header = test_header(0, 1_000_000);
        let first_tx = Bytes::new(); // Empty for genesis

        let result = l2_block_to_block_info(&config, &header, genesis_hash, &first_tx);
        assert!(result.is_ok());

        let info = result.unwrap();
        assert_eq!(info.block_info.number, 0);
        assert_eq!(info.l1_origin.number, 100);
        assert_eq!(info.l1_origin.hash, config.genesis.l1.hash);
        assert_eq!(info.seq_num, 0);
    }

    #[test]
    fn test_genesis_hash_mismatch() {
        let mut config = default_rollup_config();
        config.genesis.l2.number = 0;
        config.genesis.l2.hash =
            b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        let header = test_header(0, 1_000_000);
        let wrong_hash = b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
        let first_tx = Bytes::new();

        let result = l2_block_to_block_info(&config, &header, wrong_hash, &first_tx);
        assert!(result.is_err());
    }

    #[test]
    fn test_non_deposit_first_tx() {
        let mut config = default_rollup_config();
        config.genesis.l2.number = 0; // So block 1 is not genesis

        let header = test_header(1, 1_000_000);
        let block_hash = header.hash_slow();

        // EIP-1559 transaction (type 2) - not a deposit
        let first_tx = Bytes::from_static(&[0x02, 0xc0]); // Minimal invalid EIP-1559 tx

        let result = l2_block_to_block_info(&config, &header, block_hash, &first_tx);
        assert!(result.is_err());
    }
}
