use std::time::SystemTime;

use alloy_eips::{Encodable2718, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::Bytes;
use arbitrary::{Arbitrary, Unstructured};
use base_common_types_chain::{BaseTxEnvelope, Block, EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
use base_common_types_payload::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use libp2p::bytes::BufMut;

use crate::actors::generator::seed::SeedGenerator;

impl SeedGenerator {
    /// Generate a random Base execution payload.
    pub(crate) fn random_valid_payload(&mut self) -> BaseExecutionPayloadEnvelope {
        let block = self.valid_block();
        let (execution_payload, _) = BaseExecutionPayload::from_block_slow(&block);
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: block.header.parent_beacon_block_root,
            execution_payload,
        }
    }

    fn valid_block(&mut self) -> Block<BaseTxEnvelope> {
        // Simulate some random data
        let data = self.random_bytes(1024 * 1024);

        // Create unstructured data with the random bytes
        let u = Unstructured::new(&data);

        // Generate a random instance of MyStruct
        let mut block: Block<BaseTxEnvelope> = Block::arbitrary_take_rest(u).unwrap();

        let transactions: Vec<Bytes> =
            block.body.transactions().map(|tx| tx.encoded_2718().into()).collect();

        let transactions_root = base_common_types_chain::proofs::ordered_trie_root_with_encoder(
            &transactions,
            |item, buf| buf.put_slice(item),
        );

        block.header.transactions_root = transactions_root;

        // We always need to set the base fee per gas to a positive value to ensure the block is
        // valid.
        block.header.base_fee_per_gas =
            Some(block.header.base_fee_per_gas.unwrap_or_default().saturating_add(1));

        let current_timestamp =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        block.header.timestamp = current_timestamp;
        block.body.withdrawals = Some(Default::default());
        block.body.ommers.clear();
        block.header.withdrawals_root = Some(EMPTY_ROOT_HASH);
        block.header.blob_gas_used = Some(0);
        block.header.excess_blob_gas = Some(0);
        block.header.parent_beacon_block_root = Some(Default::default());
        block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
        block.header.ommers_hash = EMPTY_OMMER_ROOT_HASH;
        block.header.difficulty = Default::default();
        block.header.nonce = Default::default();
        block.header.block_access_list_hash = None;
        block.header.slot_number = None;

        block
    }
}
