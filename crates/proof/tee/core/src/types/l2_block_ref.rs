//! L2 block reference parsing for L1 origin validation.

use alloy_consensus::Header;
use alloy_eips::{BlockNumHash, Typed2718, eip2718::Decodable2718};
use alloy_primitives::{B256, Bytes};
use base_alloy_consensus::OpTxEnvelope;
use base_consensus_genesis::RollupConfig;
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use thiserror::Error;

/// Errors from L2 block reference parsing.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum L2BlockRefError {
    /// Genesis block hash mismatch.
    #[error("genesis hash mismatch: expected {expected}, got {actual}")]
    GenesisHashMismatch {
        /// Expected hash.
        expected: B256,
        /// Actual hash.
        actual: B256,
    },

    /// First transaction is not a deposit.
    #[error("first transaction is not a deposit, type: {0}")]
    NotDeposit(u8),

    /// Transaction decoding failed.
    #[error("failed to decode transaction: {0}")]
    TxDecodeFailed(String),

    /// Failed to decode L1 block info.
    #[error("failed to decode L1BlockInfoTx: {0}")]
    L1InfoDecodeFailed(String),
}

/// Parse `L2BlockInfo` from a block's header and first transaction.
///
/// This matches Go's `derive.L2BlockToBlockRef()` function which extracts
/// the L1 origin information from an L2 block.
pub fn l2_block_to_block_info(
    rollup_config: &RollupConfig,
    header: &Header,
    block_hash: B256,
    first_tx: &Bytes,
) -> Result<L2BlockInfo, L2BlockRefError> {
    let block_info =
        BlockInfo::new(block_hash, header.number, header.parent_hash, header.timestamp);

    // Handle genesis block
    if header.number == rollup_config.genesis.l2.number {
        if block_hash != rollup_config.genesis.l2.hash {
            return Err(L2BlockRefError::GenesisHashMismatch {
                expected: rollup_config.genesis.l2.hash,
                actual: block_hash,
            });
        }

        return Ok(L2BlockInfo::new(
            block_info,
            BlockNumHash {
                hash: rollup_config.genesis.l1.hash,
                number: rollup_config.genesis.l1.number,
            },
            0,
        ));
    }

    // Non-genesis block: decode the deposit tx and parse L1BlockInfo
    let tx = OpTxEnvelope::decode_2718(&mut first_tx.as_ref())
        .map_err(|e| L2BlockRefError::TxDecodeFailed(e.to_string()))?;

    let deposit = match &tx {
        OpTxEnvelope::Deposit(d) => d,
        _ => {
            return Err(L2BlockRefError::NotDeposit(tx.ty()));
        }
    };

    let l1_info = L1BlockInfoTx::decode_calldata(deposit.input.as_ref())
        .map_err(|e| L2BlockRefError::L1InfoDecodeFailed(e.to_string()))?;

    Ok(L2BlockInfo::new(block_info, l1_info.id(), l1_info.sequence_number()))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

    use super::*;

    fn test_header(number: u64, timestamp: u64) -> Header {
        Header {
            parent_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            number,
            gas_limit: 30_000_000,
            timestamp,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn test_genesis_block() {
        let mut config = RollupConfig::default();
        let genesis_hash =
            b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = genesis_hash;
        config.genesis.l1.number = 100;
        config.genesis.l1.hash =
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let header = test_header(0, 1_000_000);
        let first_tx = Bytes::new();

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
        let mut config = RollupConfig::default();
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
        let mut config = RollupConfig::default();
        config.genesis.l2.number = 0;

        let header = test_header(1, 1_000_000);
        let block_hash = header.hash_slow();

        let first_tx = Bytes::from_static(&[0x02, 0xc0]);

        let result = l2_block_to_block_info(&config, &header, block_hash, &first_tx);
        assert!(result.is_err());
    }
}
