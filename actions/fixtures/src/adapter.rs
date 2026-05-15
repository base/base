use alloy_consensus::{BlockBody, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::ChainGenesis;
use base_protocol::L2BlockInfo;

use base_action_harness::{L1Block, SharedL1Chain};

use crate::{ActionFixture, FixtureL1Block, FixtureL2Block};

/// Converts validated fixture data into action-harness provider inputs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionFixtureAdapter;

impl ActionFixtureAdapter {
    /// Convert captured L1 blocks into a [`SharedL1Chain`].
    pub fn shared_l1_chain(fixture: &ActionFixture) -> Result<SharedL1Chain, FixtureAdapterError> {
        let mut blocks = Vec::with_capacity(fixture.l1_blocks.len());
        for block in &fixture.l1_blocks {
            blocks.push(Self::l1_block(block)?);
        }
        Ok(SharedL1Chain::from_blocks(blocks))
    }

    /// Convert a captured L1 fixture block into the harness `L1Block` shape.
    pub fn l1_block(block: &FixtureL1Block) -> Result<L1Block, FixtureAdapterError> {
        let mut transactions = Vec::with_capacity(block.transactions.len());
        for (index, raw) in block.transactions.iter().enumerate() {
            let envelope = TxEnvelope::decode_2718_exact(raw.as_ref()).map_err(|source| {
                FixtureAdapterError::TransactionDecode {
                    chain: "l1",
                    block_number: block.header.number,
                    transaction_index: index,
                    error: source.to_string(),
                }
            })?;
            transactions.push(envelope);
        }

        let blob_sidecars =
            block.blobs.iter().map(|blob| (blob.versioned_hash, Box::new(blob.data))).collect();

        Ok(L1Block {
            header: block.header.clone(),
            transactions,
            receipts: block.receipts.clone(),
            transaction_receipts: Vec::new(),
            blob_sidecars,
        })
    }

    /// Convert a captured L2 fixture block into a Base consensus block.
    pub fn l2_block(block: &FixtureL2Block) -> Result<BaseBlock, FixtureAdapterError> {
        let mut transactions = Vec::with_capacity(block.transactions.len());
        for (index, raw) in block.transactions.iter().enumerate() {
            let envelope = BaseTxEnvelope::decode_2718(&mut raw.as_ref()).map_err(|source| {
                FixtureAdapterError::TransactionDecode {
                    chain: "l2",
                    block_number: block.header.number,
                    transaction_index: index,
                    error: source.to_string(),
                }
            })?;
            transactions.push(envelope);
        }

        Ok(BaseBlock::new(
            block.header.clone(),
            BlockBody { transactions, ommers: Vec::new(), withdrawals: None },
        ))
    }

    /// Convert a captured L2 fixture block into [`L2BlockInfo`].
    pub fn l2_block_info(
        block: &FixtureL2Block,
        genesis: &ChainGenesis,
    ) -> Result<L2BlockInfo, FixtureAdapterError> {
        let block = Self::l2_block(block)?;
        L2BlockInfo::from_block_and_genesis(&block, genesis).map_err(|source| {
            FixtureAdapterError::L2BlockInfo {
                block_number: block.header.number,
                error: source.to_string(),
            }
        })
    }
}

/// Fixture-to-harness adapter failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixtureAdapterError {
    /// A captured L1 transaction could not be decoded.
    #[error("{chain} block {block_number} tx {transaction_index} failed to decode: {error}")]
    TransactionDecode {
        /// Chain label.
        chain: &'static str,
        /// L1 block number.
        block_number: u64,
        /// Transaction index in the block.
        transaction_index: usize,
        /// Decode error text.
        error: String,
    },
    /// A captured L2 block could not be converted to `L2BlockInfo`.
    #[error("l2 block {block_number} failed to convert to L2BlockInfo: {error}")]
    L2BlockInfo {
        /// L2 block number.
        block_number: u64,
        /// Conversion error text.
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;

    use crate::{ActionFixtureAdapter, FixtureL1Block, FixtureL2Block};

    #[test]
    fn converts_empty_l1_block() {
        let block = FixtureL1Block {
            header: Header::default(),
            transactions: vec![],
            receipts: vec![],
            blobs: vec![],
        };
        let converted = ActionFixtureAdapter::l1_block(&block).unwrap();
        assert_eq!(converted.transactions.len(), 0);
    }

    #[test]
    fn converts_empty_l2_block() {
        let block = FixtureL2Block {
            header: Header::default(),
            transactions: vec![],
            receipts: vec![],
            l1_origin: None,
        };
        let converted = ActionFixtureAdapter::l2_block(&block).unwrap();
        assert_eq!(converted.body.transactions.len(), 0);
    }
}
