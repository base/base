//! Legacy channel fixtures for derivation tests.
//!
//! The production batcher emits Brotli-compressed Single batches only. These
//! helpers construct legacy payloads so action tests retain historical
//! derivation coverage without keeping legacy producers in the batcher.
//! They bypass the batcher and transaction manager, submit calldata directly,
//! and derive deterministic test channel IDs from the first transaction nonce.

use alloy_rlp::Encodable;
use base_batcher_encoder::{BatchComposer, FrameEncoder, test_utils::ChannelFramer};
use base_common_consensus::BaseBlock;
use base_protocol::{BatchType, L1BlockInfoTx, SpanBatch};
use miniz_oxide::deflate::compress_to_vec_zlib;

use crate::{ActionTestHarness, BatcherConfig};

const LEGACY_ZLIB_LEVEL: u8 = 9;

impl ActionTestHarness {
    /// Encodes and submits one pre-Fjord Single batch through zlib calldata.
    pub fn submit_single_batch_zlib_calldata(
        &mut self,
        config: &BatcherConfig,
        block: &BaseBlock,
        nonce: u64,
    ) -> eyre::Result<()> {
        let batch = BatchComposer::block_to_single_batch(block)?;
        let mut channel_input = Vec::new();
        batch.rlp_header().encode(&mut channel_input);
        channel_input.push(BatchType::Single as u8);
        batch.encode(&mut channel_input);

        let channel_data = compress_to_vec_zlib(&channel_input, LEGACY_ZLIB_LEVEL);
        self.submit_channel_fixture_calldata(config, channel_data, nonce)
    }

    /// Encodes and submits one Brotli-compressed Span fixture through calldata.
    pub fn submit_span_batch_brotli_calldata(
        &mut self,
        config: &BatcherConfig,
        blocks: &[BaseBlock],
        first_nonce: u64,
    ) -> eyre::Result<()> {
        let channel_input = self.encode_span_batch_channel_input(blocks)?;
        let channel_data = config.encoder.brotli_level.compress_channel(&channel_input)?;
        self.submit_channel_fixture_calldata(config, channel_data, first_nonce)
    }

    /// Encodes and submits one legacy zlib-compressed Span fixture through calldata.
    pub fn submit_span_batch_zlib_calldata(
        &mut self,
        config: &BatcherConfig,
        blocks: &[BaseBlock],
        first_nonce: u64,
    ) -> eyre::Result<()> {
        let channel_input = self.encode_span_batch_channel_input(blocks)?;
        let channel_data = compress_to_vec_zlib(&channel_input, LEGACY_ZLIB_LEVEL);
        self.submit_channel_fixture_calldata(config, channel_data, first_nonce)
    }

    /// Encodes blocks into the uncompressed channel input for one Span batch.
    pub fn encode_span_batch_channel_input(&self, blocks: &[BaseBlock]) -> eyre::Result<Vec<u8>> {
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

        Ok(channel_input)
    }

    /// Frames and submits compressed channel data through calldata, then mines one L1 block.
    pub fn submit_channel_fixture_calldata(
        &mut self,
        config: &BatcherConfig,
        channel_data: Vec<u8>,
        first_nonce: u64,
    ) -> eyre::Result<()> {
        let mut channel_id = [0u8; 16];
        channel_id[8..].copy_from_slice(&first_nonce.to_be_bytes());
        let frames = ChannelFramer::split(channel_id, channel_data, config.encoder.max_frame_size)?;

        for (offset, frame) in frames.iter().enumerate() {
            self.l1.submit_calldata_transaction(
                &config.l1_signer,
                self.rollup_config.l1_chain_id,
                first_nonce + offset as u64,
                config.inbox_address,
                FrameEncoder::to_calldata(frame),
            )?;
        }
        self.l1.mine_block();
        Ok(())
    }
}
