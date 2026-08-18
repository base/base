//! Span-batch fixtures for derivation tests.
//!
//! The production batcher emits Single batches only. These helpers construct
//! protocol-level Span payloads so action tests can continue covering Span
//! decoding and derivation without retaining a Span producer.

use alloy_rlp::Encodable;
use base_batcher_encoder::{BatchComposer, FrameEncoder, test_utils::ChannelFramer};
use base_common_consensus::BaseBlock;
use base_protocol::{BatchType, L1BlockInfoTx, SpanBatch};

use crate::{ActionTestHarness, BatcherConfig, L1TxBuilder};

impl ActionTestHarness {
    /// Encodes and submits one protocol-level Span fixture through calldata.
    pub fn submit_span_batch_calldata(
        &mut self,
        config: &BatcherConfig,
        blocks: &[BaseBlock],
        first_nonce: u64,
    ) -> eyre::Result<()> {
        let mut channel_id = [0u8; 16];
        channel_id[8..].copy_from_slice(&first_nonce.to_be_bytes());
        let mut span = SpanBatch {
            chain_id: self.rollup_config.l2_chain_id.id(),
            genesis_timestamp: self.rollup_config.genesis.l2_time,
            ..Default::default()
        };
        for block in blocks {
            let single = BatchComposer::block_to_single_batch(block)?;
            let Some(deposit) = block.body.transactions.first().and_then(|tx| tx.as_deposit())
            else {
                eyre::bail!("span fixture block has no L1 info deposit");
            };
            let l1_info = L1BlockInfoTx::decode_calldata(&deposit.input)?;
            span.append_singular_batch(single, l1_info.sequence_number())?;
        }

        let mut encoded_span = vec![BatchType::Span as u8];
        span.encode(&mut encoded_span)?;
        let mut channel_input = Vec::new();
        encoded_span.as_slice().encode(&mut channel_input);

        let channel_data = config.encoder.compression_algo.compress_channel(&channel_input)?;
        let frames = ChannelFramer::split(channel_id, channel_data, config.encoder.max_frame_size)?;

        for (offset, frame) in frames.iter().enumerate() {
            let transaction = L1TxBuilder::signed_calldata(
                &config.l1_signer,
                self.rollup_config.l1_chain_id,
                first_nonce + offset as u64,
                config.inbox_address,
                FrameEncoder::to_calldata(frame),
            )?;
            self.l1.submit_transaction(transaction);
        }
        self.l1.mine_block();
        Ok(())
    }
}
