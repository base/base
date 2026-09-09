//! This module contains the `ChannelReader` struct.

use alloc::boxed::Box;
use core::fmt::Debug;

use alloy_eips::BlockNumHash;
use alloy_primitives::Bytes;
use async_trait::async_trait;
use base_common_chain_config::{RollupConfig, SystemConfig};
use base_protocol::{BatchReader, BlockInfo, SingleBatch};
use tracing::{debug, warn};

use crate::{
    Metrics, NextBatchProvider, OriginAdvancer, OriginProvider, PipelineError, PipelineResult,
    StageReset,
};

/// The [`ChannelReader`] provider trait.
#[async_trait]
pub trait ChannelReaderProvider {
    /// Pulls the next piece of data from the channel bank. Note that it attempts to pull data out
    /// of the channel bank prior to loading data in (unlike most other stages). This is to
    /// ensure maintain consistency around channel bank pruning which depends upon the order
    /// of operations.
    async fn next_data(&mut self) -> PipelineResult<Option<Bytes>>;
}

/// [`ChannelReader`] is a stateful stage that reads [`SingleBatch`]es from `Channel`s.
///
/// The [`ChannelReader`] pulls `Channel`s from the channel bank as raw data
/// and pipes it into a `BatchReader`. Since the raw data is compressed,
/// the `BatchReader` first decompresses the data using the first bytes as
/// a compression algorithm identifier.
///
/// Once the data is decompressed, it is decoded into a `SingleBatch` and passed
/// to the next stage in the pipeline.
#[derive(Debug)]
pub struct ChannelReader<P>
where
    P: ChannelReaderProvider + OriginAdvancer + OriginProvider + StageReset + Debug,
{
    /// The previous stage of the derivation pipeline.
    pub prev: P,
    /// The batch reader.
    pub next_batch: Option<BatchReader>,
}

impl<P> ChannelReader<P>
where
    P: ChannelReaderProvider + OriginAdvancer + OriginProvider + StageReset + Debug,
{
    /// Create a new [`ChannelReader`] stage.
    pub const fn new(prev: P) -> Self {
        Self { prev, next_batch: None }
    }

    /// Creates the batch reader from available channel data.
    async fn set_batch_reader(&mut self) -> PipelineResult<()> {
        if self.next_batch.is_none() {
            let channel =
                self.prev.next_data().await?.ok_or(PipelineError::ChannelReaderEmpty.temp())?;

            let _origin = self.prev.origin().ok_or(PipelineError::MissingOrigin.crit())?;
            self.next_batch = Some(BatchReader::new(
                &channel[..],
                RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize,
            ));
            Metrics::pipeline_batch_reader_set().set(1);
        }
        Ok(())
    }

    /// Forces the read to continue with the next channel, resetting any
    /// decoding / decompression state to a fresh start.
    pub fn next_channel(&mut self) {
        self.next_batch = None;
        Metrics::pipeline_batch_reader_set().set(0);
    }
}

#[async_trait]
impl<P> OriginAdvancer for ChannelReader<P>
where
    P: ChannelReaderProvider + OriginAdvancer + OriginProvider + StageReset + Send + Debug,
{
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        self.prev.advance_origin().await
    }
}

#[async_trait]
impl<P> NextBatchProvider for ChannelReader<P>
where
    P: ChannelReaderProvider + OriginAdvancer + OriginProvider + StageReset + Send + Debug,
{
    /// Discards the current channel after an invalid batch.
    fn flush(&mut self) {
        debug!(target: "channel_reader", "Flushing channel");
        self.next_channel();
    }

    async fn next_batch(&mut self) -> PipelineResult<SingleBatch> {
        if let Err(e) = self.set_batch_reader().await {
            debug!(target: "channel_reader", error = ?e, "Failed to set batch reader");
            self.next_channel();
            return Err(e);
        }

        // SAFETY: The batch reader must be set above.
        let next_batch = self.next_batch.as_mut().expect("SingleBatch reader must be set");
        let decompress_result =
            base_metrics::time!(Metrics::pipeline_batch_decompress_duration_seconds(), {
                next_batch.decompress()
            });
        match decompress_result {
            Ok(()) => {
                // Record the decompressed size and type.
                let size = next_batch.decompressed.len() as f64;
                let ty = if next_batch.brotli_used {
                    BatchReader::CHANNEL_VERSION_BROTLI
                } else {
                    BatchReader::ZLIB_DEFLATE_COMPRESSION_METHOD
                };
                Metrics::pipeline_latest_decompressed_batch_size().set(size);
                Metrics::pipeline_latest_decompressed_batch_type().set(ty as f64);
            }
            Err(err) => {
                debug!(target: "channel_reader", ?err, "Failed to decompress batch");
                self.next_channel();
                return Err(PipelineError::NotEnoughData.temp());
            }
        }

        // Read the next batch from the reader's decompressed data
        let batch_result =
            base_metrics::time!(Metrics::pipeline_batch_decode_duration_seconds(), {
                next_batch.next_batch()
            });
        match batch_result.ok_or(PipelineError::NotEnoughData.temp()) {
            Ok(batch) => {
                Metrics::pipeline_read_batches().increment(1.0);
                Ok(batch)
            }
            Err(e) => {
                self.next_channel();
                Err(e)
            }
        }
    }
}

impl<P> OriginProvider for ChannelReader<P>
where
    P: ChannelReaderProvider + OriginAdvancer + OriginProvider + StageReset + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.prev.origin()
    }
}

#[async_trait]
impl<P> StageReset for ChannelReader<P>
where
    P: ChannelReaderProvider + OriginAdvancer + OriginProvider + StageReset + Debug + Send,
{
    async fn reset(
        &mut self,
        l1_origin: BlockNumHash,
        system_config: SystemConfig,
    ) -> PipelineResult<()> {
        self.prev.reset(l1_origin, system_config).await?;
        self.next_channel();
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        self.prev.activate().await?;
        self.next_channel();
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        // Drop the current in-progress channel. Does NOT propagate to prev.
        warn!(target: "channel_reader", "Flushed channel");
        self.next_batch = None;
        Metrics::pipeline_batch_reader_set().set(0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_eips::BlockNumHash;
    use alloy_rlp::Encodable;
    use base_common_chain_config::SystemConfig;

    use super::*;
    use crate::{errors::PipelineErrorKind, test_utils::TestChannelReaderProvider};

    fn new_compressed_batch_data() -> Bytes {
        let file_contents =
            alloc::string::String::from_utf8_lossy(include_bytes!("../../../testdata/batch.hex"));
        let file_contents = &(&*file_contents)[..file_contents.len() - 1];
        let data = alloy_primitives::hex::decode(file_contents).unwrap();
        data.into()
    }

    #[tokio::test]
    async fn test_flush_channel_reader() {
        let mock = TestChannelReaderProvider::new(vec![Ok(Some(new_compressed_batch_data()))]);
        let mut reader = ChannelReader::new(mock);
        reader.next_batch = Some(BatchReader::new(
            new_compressed_batch_data(),
            RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize,
        ));
        reader.flush_channel().await.unwrap();
        assert!(reader.next_batch.is_none());
    }

    #[tokio::test]
    async fn test_reset_channel_reader() {
        let mock = TestChannelReaderProvider::new(vec![Ok(None)]);
        let mut reader = ChannelReader::new(mock);
        reader.next_batch = Some(BatchReader::new(
            vec![0x00, 0x01, 0x02],
            RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize,
        ));
        assert!(!reader.prev.reset);
        reader.reset(BlockNumHash::default(), SystemConfig::default()).await.unwrap();
        assert!(reader.next_batch.is_none());
        assert!(reader.prev.reset);
    }

    #[tokio::test]
    async fn test_next_batch_batch_reader_set_fails() {
        let mock = TestChannelReaderProvider::new(vec![Err(PipelineError::Eof.temp())]);
        let mut reader = ChannelReader::new(mock);
        assert_eq!(reader.next_batch().await, Err(PipelineError::Eof.temp()));
        assert!(reader.next_batch.is_none());
    }

    #[tokio::test]
    async fn test_next_batch_batch_reader_no_data() {
        let mock = TestChannelReaderProvider::new(vec![Ok(None)]);
        let mut reader = ChannelReader::new(mock);
        assert!(matches!(
            reader.next_batch().await.unwrap_err(),
            PipelineErrorKind::Temporary(PipelineError::ChannelReaderEmpty)
        ));
        assert!(reader.next_batch.is_none());
    }

    #[tokio::test]
    async fn test_next_batch_batch_reader_not_enough_data() {
        let mut first = new_compressed_batch_data();
        let second = first.split_to(first.len() / 2);
        let mock = TestChannelReaderProvider::new(vec![Ok(Some(first)), Ok(Some(second))]);
        let mut reader = ChannelReader::new(mock);
        assert_eq!(reader.next_batch().await, Err(PipelineError::NotEnoughData.temp()));
        assert!(reader.next_batch.is_none());
    }

    #[tokio::test]
    async fn test_next_batch_succeeds() {
        let raw = new_compressed_batch_data();
        let mock = TestChannelReaderProvider::new(vec![Ok(Some(raw))]);
        let mut reader = ChannelReader::new(mock);
        let res = reader.next_batch().await.unwrap();
        assert!(!res.transactions.is_empty());
        assert!(reader.next_batch.is_some());
    }

    #[tokio::test]
    async fn test_flush_post_holocene() {
        let raw = new_compressed_batch_data();
        let mock = TestChannelReaderProvider::new(vec![Ok(Some(raw))]);
        let mut reader = ChannelReader::new(mock);
        let res = reader.next_batch().await.unwrap();
        assert!(!res.transactions.is_empty());
        assert!(reader.next_batch.is_some());
        reader.flush();
        assert!(reader.next_batch.is_none());
    }

    #[tokio::test]
    async fn unsupported_span_flushes_channel_and_next_singular_channel_recovers() {
        let mut channel = Vec::new();
        [1u8].as_slice().encode(&mut channel);
        let ignored = SingleBatch { timestamp: 999, ..Default::default() };
        let mut encoded = Vec::new();
        ignored.encode_batch(&mut encoded);
        encoded.as_slice().encode(&mut channel);

        let expected_channel = new_compressed_batch_data();
        let expected = BatchReader::new(expected_channel.clone(), 10_000_000).next_batch().unwrap();
        let provider = TestChannelReaderProvider::new(vec![Ok(Some(expected_channel))]);
        let mut reader = ChannelReader::new(provider);
        let mut buffered = BatchReader::new(Vec::new(), 10_000_000);
        buffered.decompressed = channel;
        reader.next_batch = Some(buffered);
        assert_eq!(reader.next_batch().await, Err(PipelineError::NotEnoughData.temp()));
        assert!(reader.next_batch.is_none(), "the entire unsupported channel must be discarded");
        assert_eq!(reader.next_batch().await.unwrap(), expected);
    }
}
