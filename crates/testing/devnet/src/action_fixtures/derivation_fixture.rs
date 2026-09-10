//! Channel fixtures for derivation rejection and compression tests.
//!
//! These helpers submit calldata directly so tests can exercise formats that
//! the production Brotli singular batcher does not emit.

use alloy_rlp::Encodable;
use base_batcher_encoding_channel::{BatchComposer, FrameEncoder, test_utils::ChannelFramer};
use base_common_types_chain::BaseBlock;
use base_consensus_batch_types::SingleBatch;
use miniz_oxide::deflate::compress_to_vec_zlib;

use crate::action_fixtures::{ActionTestHarness, BatcherConfig};

const LEGACY_ZLIB_LEVEL: u8 = 9;

impl ActionTestHarness {
    /// Submits a channel with the retired span discriminator for rejection tests.
    pub fn submit_unsupported_span_calldata(
        &mut self,
        config: &BatcherConfig,
        nonce: u64,
    ) -> eyre::Result<()> {
        let mut input = Vec::new();
        [1u8].as_slice().encode(&mut input);
        let channel = config.encoder.brotli_level.compress_channel(&input)?;
        self.submit_channel_fixture_calldata(config, channel, nonce)
    }

    /// Encodes and submits one singular batch through zlib calldata.
    pub fn submit_single_batch_zlib_calldata(
        &mut self,
        config: &BatcherConfig,
        block: &BaseBlock,
        nonce: u64,
    ) -> eyre::Result<()> {
        let batch = BatchComposer::block_to_single_batch(block)?;
        let mut channel_input = Vec::new();
        batch.rlp_header().encode(&mut channel_input);
        channel_input.push(SingleBatch::TYPE);
        batch.encode(&mut channel_input);

        let channel_data = compress_to_vec_zlib(&channel_input, LEGACY_ZLIB_LEVEL);
        self.submit_channel_fixture_calldata(config, channel_data, nonce)
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
