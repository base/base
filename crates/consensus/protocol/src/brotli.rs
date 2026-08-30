//! Contains brotli decompression utilities.

use alloc::{vec, vec::Vec};
use core::ops;

use alloc_no_stdlib::{
    AllocatedStackMemory, Allocator, SliceWrapper, SliceWrapperMut, StackAllocator, bzero,
    declare_stack_allocator_struct, define_stack_allocator_traits, static_array,
};
use brotli::{BrotliResult, BrotliState, HuffmanCode};

/// A brotli decompression error.
#[derive(thiserror::Error, Debug)]
pub enum BrotliDecompressionError {
    /// Brotli decompression failed due to corrupt or invalid data.
    #[error("brotli decompression failed: {0:?}")]
    DecompressionFailed(BrotliResult),
}

/// A reusable brotli decompressor.
///
/// Owns the scratch memory the brotli decoder allocates from. The decoder is handed a
/// stack allocator over three fixed pools rather than a general-purpose heap allocator,
/// so the pools must be sized for brotli's large-window worst case: a 32 `MiB` ring
/// buffer, 4 `MiB` of `u32` tables, and 16 `MiB` of Huffman tables, ~52 `MiB` in total. None
/// of that depends on the channel being decompressed, so it is allocated once on first
/// use and reused for the lifetime of the decompressor.
///
/// Hold one per derivation pipeline. Constructing a decompressor per channel reintroduces
/// the ~52 `MiB` allocation on the critical path, which is what this type exists to avoid.
pub struct Brotli {
    /// Ring-buffer and context-map pool.
    u8_pool: Vec<u8>,
    /// Huffman-table offset pool.
    u32_pool: Vec<u32>,
    /// Huffman-code pool.
    hc_pool: Vec<HuffmanCode>,
}

impl Brotli {
    /// Length of the `u8` scratch pool. Brotli's ring buffer is sized from the stream's
    /// window header, and large-window streams are accepted, so this is the maximum.
    pub const U8_POOL_LEN: usize = 32 * 1024 * 1024;

    /// Length of the `u32` scratch pool, in elements.
    pub const U32_POOL_LEN: usize = 1024 * 1024;

    /// Length of the [`HuffmanCode`] scratch pool, in elements.
    pub const HC_POOL_LEN: usize = 4 * 1024 * 1024;

    /// Creates a decompressor. The scratch pools are allocated on the first call to
    /// [`Self::decompress`], so this is free and holders pay nothing until they
    /// actually decompress a brotli channel.
    pub const fn new() -> Self {
        Self { u8_pool: Vec::new(), u32_pool: Vec::new(), hc_pool: Vec::new() }
    }

    /// Borrows the three scratch pools, allocating them if this is the first call.
    fn pools(&mut self) -> (&mut [u8], &mut [u32], &mut [HuffmanCode]) {
        if self.u8_pool.is_empty() {
            self.u8_pool = vec![0; Self::U8_POOL_LEN];
            self.u32_pool = vec![0; Self::U32_POOL_LEN];
            self.hc_pool = vec![HuffmanCode::default(); Self::HC_POOL_LEN];
        }
        (&mut self.u8_pool, &mut self.u32_pool, &mut self.hc_pool)
    }

    /// Decompresses the given bytes data using the Brotli decompressor implemented
    /// in the [`brotli`](https://crates.io/crates/brotli) crate.
    pub fn decompress(
        &mut self,
        data: &[u8],
        max_rlp_bytes_per_channel: usize,
    ) -> Result<Vec<u8>, BrotliDecompressionError> {
        declare_stack_allocator_struct!(MemPool, 4096, stack);

        // Reuse is safe because the decoder initialises every cell it reads, so bytes left
        // behind by a previous channel cannot reach the output.
        // `test_decompress_ignores_stale_scratch` pins that by poisoning the pools between
        // calls, and holds even with the `bzero` initialiser below replaced by a no-op.
        let (u8_pool, u32_pool, hc_pool) = self.pools();
        let u8_allocator = MemPool::<u8>::new_allocator(u8_pool, bzero);
        let u32_allocator = MemPool::<u32>::new_allocator(u32_pool, bzero);
        let hc_allocator = MemPool::<HuffmanCode>::new_allocator(hc_pool, bzero);
        let mut brotli_state = BrotliState::new(u8_allocator, u32_allocator, hc_allocator);

        // Setup the decompressor inputs and outputs.
        // Cap initial buffer at the limit to prevent over-allocation.
        let mut output = vec![0; core::cmp::min(data.len(), max_rlp_bytes_per_channel)];
        let mut available_in = data.len();
        let mut input_offset = 0;
        let mut available_out = output.len();
        let mut output_offset = 0;
        let mut written = 0;

        // Decompress the data stream until success or failure.
        // The output buffer is grown as needed, capped at max_rlp_bytes_per_channel.
        // Per spec, if decompressed data exceeds the limit, the output is truncated
        // to max_rlp_bytes_per_channel bytes (not rejected).
        loop {
            let result = brotli::BrotliDecompressStream(
                &mut available_in,
                &mut input_offset,
                data,
                &mut available_out,
                &mut output_offset,
                &mut output,
                &mut written,
                &mut brotli_state,
            );
            let old_len = output.len();

            match result {
                // Buffer was already grown to the limit on a previous iteration, but the decompressor
                // filled it and still has more to produce: stop per spec.
                BrotliResult::NeedsMoreOutput if old_len >= max_rlp_bytes_per_channel => break,
                // Enlarge output buffer to continue decompression.
                BrotliResult::NeedsMoreOutput => {
                    let new_len = core::cmp::min((old_len * 2).max(1), max_rlp_bytes_per_channel);
                    output.resize(new_len, 0);
                    available_out += new_len - old_len;
                }
                // No output: error.
                _ if written == 0 => {
                    return Err(BrotliDecompressionError::DecompressionFailed(result));
                }
                // Success, NeedsMoreInput or ResultFailure with some output written: return partial
                // data.
                _ => break,
            }
        }

        output.truncate(written);
        Ok(output)
    }
}

impl Default for Brotli {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Brotli {
    /// Reports whether the scratch pools are live rather than their tens of megabytes
    /// of contents, since [`Brotli`] is reachable from the `Debug` of derivation stages.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Brotli").field("scratch_allocated", &!self.u8_pool.is_empty()).finish()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use base_common_genesis::RollupConfig;

    use super::*;

    /// A real brotli-compressed mainnet channel, ~12 `KiB`.
    fn mainnet_channel() -> Vec<u8> {
        hex::decode(include_str!("../testdata/channel_brotli.hex").trim_end()).unwrap()
    }

    /// The expected decompression of [`mainnet_channel`].
    fn mainnet_channel_decompressed() -> Vec<u8> {
        hex::decode(include_str!("../testdata/channel_brotli_decompressed.hex").trim_end()).unwrap()
    }

    /// Brotli-compresses `data` at the library defaults.
    fn compress(data: &[u8]) -> Vec<u8> {
        let params = brotli::enc::BrotliEncoderParams::default();
        let mut output = Vec::new();
        brotli::BrotliCompress(&mut &data[..], &mut output, &params).unwrap();
        output
    }

    #[test]
    fn test_brotli_decompress() {
        let expected = hex!("75ed184249e9bc19675e");
        let compressed = hex!("8b048075ed184249e9bc19675e03");

        let decompressed = Brotli::new()
            .decompress(&compressed, RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn test_decompress_batch_brotli() {
        let decompressed = Brotli::new()
            .decompress(&mainnet_channel(), RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, mainnet_channel_decompressed());
    }

    #[test]
    fn test_scratch_is_allocated_lazily_and_only_once() {
        let mut brotli = Brotli::new();
        assert!(brotli.u8_pool.is_empty(), "constructing a decompressor must not allocate");

        brotli
            .decompress(&mainnet_channel(), RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        let pool_addr = brotli.u8_pool.as_ptr();
        assert_eq!(brotli.u8_pool.len(), Brotli::U8_POOL_LEN);
        assert_eq!(brotli.u32_pool.len(), Brotli::U32_POOL_LEN);
        assert_eq!(brotli.hc_pool.len(), Brotli::HC_POOL_LEN);

        brotli
            .decompress(&mainnet_channel(), RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(
            brotli.u8_pool.as_ptr(),
            pool_addr,
            "a second decompression must reuse the same scratch allocation"
        );
    }

    #[test]
    fn test_decompress_ignores_stale_scratch() {
        let limit = RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize;
        let channel = mainnet_channel();
        let expected = mainnet_channel_decompressed();

        let mut brotli = Brotli::new();
        brotli.decompress(&channel, limit).unwrap();

        // Poison every pool so any read of scratch the decoder has not written itself
        // produces a different answer. Zero-filled scratch, which is what a freshly
        // allocated pool happens to provide, would hide such a read.
        brotli.u8_pool.fill(0xFF);
        brotli.u32_pool.fill(u32::MAX);
        brotli.hc_pool.fill(HuffmanCode { bits: 0xFF, value: u16::MAX });

        assert_eq!(brotli.decompress(&channel, limit).unwrap(), expected);
    }

    #[test]
    fn test_reused_decompressor_matches_fresh_decompressor() {
        let limit = RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize;

        // Channels of differing shape and size, so consecutive calls leave scratch in a
        // state the next call cannot have produced for itself.
        let payloads: Vec<Vec<u8>> = alloc::vec![
            mainnet_channel_decompressed(),
            alloc::vec![0xAA; 100],
            alloc::vec![0u8; 512 * 1024],
            (0..64u32).flat_map(u32::to_be_bytes).collect(),
            alloc::vec![0x5C; 1],
        ];
        let channels: Vec<Vec<u8>> = payloads.iter().map(|p| compress(p)).collect();

        // Sweep forwards then backwards through one decompressor, so every channel is
        // decompressed against scratch left behind by a larger and by a smaller channel.
        let mut brotli = Brotli::new();
        for index in (0..channels.len()).chain((0..channels.len()).rev()) {
            let reused = brotli.decompress(&channels[index], limit).unwrap();
            assert_eq!(
                reused, payloads[index],
                "channel {index} decompressed differently on a reused decompressor"
            );
        }
    }

    #[test]
    fn test_brotli_truncation_instead_of_rejection() {
        // Use the small test data to verify truncation behavior.
        let expected = hex!("75ed184249e9bc19675e");
        let compressed = hex!("8b048075ed184249e9bc19675e03");
        let full_len = expected.len();
        let mut brotli = Brotli::new();

        // Full limit — should decompress fully.
        let decompressed = brotli.decompress(&compressed, full_len).unwrap();
        assert_eq!(decompressed, expected);

        // Limit smaller than data — should truncate, not error.
        let limit = full_len / 2;
        let decompressed = brotli.decompress(&compressed, limit).unwrap();
        assert!(
            decompressed.len() <= limit,
            "truncated output ({}) should not exceed limit ({})",
            decompressed.len(),
            limit
        );
    }

    #[test]
    fn test_brotli_buffer_doubling_regression() {
        // Regression test for the buffer-doubling bug: the old code doubled the
        // output buffer and rejected if the doubled size exceeded the limit,
        // even if the actual decompressed data fit within the limit.
        //
        // Example: 100 bytes of data, initial buffer = compressed.len() (small),
        // buffer doubles past 100 -> old code returns BatchTooLarge.
        let data = alloc::vec![0xAA; 100];
        let compressed = compress(&data);
        let mut brotli = Brotli::new();

        // Limit = exact decompressed size. The old code would error because
        // internal buffer doubling would overshoot.
        let result = brotli.decompress(&compressed, data.len());
        assert!(result.is_ok(), "decompression at exact limit should succeed");
        assert_eq!(result.unwrap(), data);

        // Limit = data.len() + 1 — also succeeds.
        let result = brotli.decompress(&compressed, data.len() + 1);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), data);
    }
}
