use std::collections::VecDeque;

use alloy_eips::eip4844::{BYTES_PER_BLOB, Blob, VERSIONED_HASH_VERSION_KZG};
use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use base_consensus_derive::{
    BlobProvider, BlobProviderError, DataAvailabilityProvider, PipelineError, PipelineResult,
};
use base_protocol::BlockInfo;

use crate::SharedL1Chain;

/// Blob encoding version used by the OP Stack blob codec.
const BLOB_ENCODING_VERSION: u8 = 0;

/// Maximum number of data bytes that fit in a single blob.
const BLOB_MAX_DATA_SIZE: usize = (4 * 31 + 3) * 1024 - 4; // 130044

/// Number of encoding rounds (one per group of 4 field elements).
const BLOB_ENCODING_ROUNDS: usize = 1024;

/// Encode frame bytes into the OP Stack blob wire format.
///
/// `frame_data` is the raw batcher payload: `[DERIVATION_VERSION_0] ++ encoded_frames`.
/// The returned [`Blob`] is the full 131 072-byte EIP-4844 blob that the pipeline's
/// `BlobData::decode()` (or [`decode_blob`]) can decode back to `frame_data`.
///
/// # Panics
///
/// Panics if `frame_data` is larger than [`BLOB_MAX_DATA_SIZE`] (130 044 bytes).
pub fn frames_to_blob(frame_data: &[u8]) -> Box<Blob> {
    assert!(
        frame_data.len() <= BLOB_MAX_DATA_SIZE,
        "frame data ({} bytes) exceeds BLOB_MAX_DATA_SIZE ({BLOB_MAX_DATA_SIZE})",
        frame_data.len(),
    );

    // Pad the input to BLOB_MAX_DATA_SIZE so the encoder always has enough data.
    let mut input = vec![0u8; BLOB_MAX_DATA_SIZE];
    input[..frame_data.len()].copy_from_slice(frame_data);

    let mut blob = Box::new(Blob::ZERO);
    let data: &mut [u8; BYTES_PER_BLOB] = blob.as_mut();

    // The OP Stack blob encoding stores payload data across field elements.
    // Each field element is 32 bytes: [high_byte | 31 payload bytes].
    // Every 4 field elements, the 4 high bytes carry 6-bit chunks that
    // reassemble into 3 additional payload bytes.
    //
    // The decode produces output in this order per round:
    //   [FE0 payload] [reassembled x] [FE1 payload] [reassembled y]
    //   [FE2 payload] [reassembled z] [FE3 payload]
    //
    // Round 0 is special: FE0 has only 27 payload bytes (5 bytes used for
    // the encoding version + 3-byte length header).

    // --- FE0 metadata ---
    let len_be = (frame_data.len() as u32).to_be_bytes();
    data[VERSIONED_HASH_VERSION_KZG as usize] = BLOB_ENCODING_VERSION;
    data[2] = len_be[1];
    data[3] = len_be[2];
    data[4] = len_be[3];

    // FE0 payload: 27 bytes.
    data[5..32].copy_from_slice(&input[0..27]);
    let mut ipos = 27usize; // input position

    // Read the 3 reassembly bytes that will be distributed across high bytes.
    let x = input[ipos];
    ipos += 1;
    // FE1 payload: 31 bytes.
    data[33..64].copy_from_slice(&input[ipos..ipos + 31]);
    ipos += 31;
    let y = input[ipos];
    ipos += 1;
    // FE2 payload: 31 bytes.
    data[65..96].copy_from_slice(&input[ipos..ipos + 31]);
    ipos += 31;
    let z = input[ipos];
    ipos += 1;
    // FE3 payload: 31 bytes.
    data[97..128].copy_from_slice(&input[ipos..ipos + 31]);
    ipos += 31;

    // Compute encoded high bytes from x, y, z (inverse of reassemble_bytes).
    let e0 = x & 0x3F;
    let e1 = ((x >> 6) & 0x03) << 4 | (y & 0x0F);
    let e2 = z & 0x3F;
    let e3 = ((y >> 4) & 0x0F) | (((z >> 6) & 0x03) << 4);

    data[0] = e0;
    data[32] = e1;
    data[64] = e2;
    data[96] = e3;

    // --- Rounds 1..1024 ---
    let mut blob_pos = 128usize; // next field element start in blob
    for _ in 1..BLOB_ENCODING_ROUNDS {
        if ipos >= frame_data.len() {
            break;
        }

        // FE0 payload (31 bytes).
        data[blob_pos + 1..blob_pos + 32].copy_from_slice(&input[ipos..ipos + 31]);
        ipos += 31;
        let x = input[ipos];
        ipos += 1;

        // FE1 payload (31 bytes).
        data[blob_pos + 33..blob_pos + 64].copy_from_slice(&input[ipos..ipos + 31]);
        ipos += 31;
        let y = input[ipos];
        ipos += 1;

        // FE2 payload (31 bytes).
        data[blob_pos + 65..blob_pos + 96].copy_from_slice(&input[ipos..ipos + 31]);
        ipos += 31;
        let z = input[ipos];
        ipos += 1;

        // FE3 payload (31 bytes).
        data[blob_pos + 97..blob_pos + 128].copy_from_slice(&input[ipos..ipos + 31]);
        ipos += 31;

        // Compute and write high bytes.
        data[blob_pos] = x & 0x3F;
        data[blob_pos + 32] = ((x >> 6) & 0x03) << 4 | (y & 0x0F);
        data[blob_pos + 64] = z & 0x3F;
        data[blob_pos + 96] = ((y >> 4) & 0x0F) | (((z >> 6) & 0x03) << 4);

        blob_pos += 128;
    }

    blob
}

/// Decode an OP Stack blob back to the raw frame bytes.
///
/// This is the inverse of [`frames_to_blob`] and mirrors the logic in
/// `BlobData::decode()` from `base-consensus-derive`.
///
/// # Panics
///
/// Panics if the blob has an invalid encoding version or length.
pub fn decode_blob(blob: &Blob) -> Bytes {
    let data: &[u8; BYTES_PER_BLOB] = blob.as_ref();

    assert_eq!(
        data[VERSIONED_HASH_VERSION_KZG as usize], BLOB_ENCODING_VERSION,
        "invalid blob encoding version"
    );

    let length = u32::from_be_bytes([0, data[2], data[3], data[4]]) as usize;
    assert!(length <= BLOB_MAX_DATA_SIZE, "blob length {length} exceeds max {BLOB_MAX_DATA_SIZE}");

    let mut output = vec![0u8; BLOB_MAX_DATA_SIZE];

    // Round 0: first 27 bytes from field element 0.
    output[0..27].copy_from_slice(&data[5..32]);

    let mut output_pos = 28usize;
    let mut input_pos = 32usize;
    let mut encoded_byte = [0u8; 4];
    encoded_byte[0] = data[0];

    for b in encoded_byte.iter_mut().skip(1) {
        *b = data[input_pos] & 0b0011_1111;
        output[output_pos..output_pos + 31].copy_from_slice(&data[input_pos + 1..input_pos + 32]);
        output_pos += 32;
        input_pos += 32;
    }

    // Reassemble round 0.
    output_pos -= 1;
    let x = (encoded_byte[0] & 0b0011_1111) | ((encoded_byte[1] & 0b0011_0000) << 2);
    let y = (encoded_byte[1] & 0b0000_1111) | ((encoded_byte[3] & 0b0000_1111) << 4);
    let z = (encoded_byte[2] & 0b0011_1111) | ((encoded_byte[3] & 0b0011_0000) << 2);
    output[output_pos - 32] = z;
    output[output_pos - 64] = y;
    output[output_pos - 96] = x;

    // Rounds 1..1024.
    for _ in 1..BLOB_ENCODING_ROUNDS {
        if output_pos >= length {
            break;
        }
        for d in &mut encoded_byte {
            *d = data[input_pos] & 0b0011_1111;
            output[output_pos..output_pos + 31]
                .copy_from_slice(&data[input_pos + 1..input_pos + 32]);
            output_pos += 32;
            input_pos += 32;
        }
        output_pos -= 1;
        let x = (encoded_byte[0] & 0b0011_1111) | ((encoded_byte[1] & 0b0011_0000) << 2);
        let y = (encoded_byte[1] & 0b0000_1111) | ((encoded_byte[3] & 0b0000_1111) << 4);
        let z = (encoded_byte[2] & 0b0011_1111) | ((encoded_byte[3] & 0b0011_0000) << 2);
        output[output_pos - 32] = z;
        output[output_pos - 64] = y;
        output[output_pos - 96] = x;
    }

    output.truncate(length);
    Bytes::from(output)
}

/// In-memory blob provider backed by [`SharedL1Chain`] blob sidecars.
///
/// Implements [`BlobProvider`] for action tests that use EIP-4844 blob
/// submission. Blobs are stored in [`L1Block::blob_sidecars`](crate::L1Block)
/// when enqueued via [`L1Miner::enqueue_blob`](crate::L1Miner::enqueue_blob)
/// and looked up here by versioned hash.
#[derive(Debug, Clone)]
pub struct ActionBlobProvider {
    chain: SharedL1Chain,
}

impl ActionBlobProvider {
    /// Create a new provider backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain) -> Self {
        Self { chain }
    }
}

#[async_trait]
impl BlobProvider for ActionBlobProvider {
    type Error = BlobProviderError;

    async fn get_and_validate_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        let block = self
            .chain
            .get_block(block_ref.number)
            .filter(|b| b.hash() == block_ref.hash)
            .ok_or_else(|| {
            BlobProviderError::Backend(format!("block {} not found in chain", block_ref.number))
        })?;

        let mut blobs = Vec::new();
        for hash in blob_hashes {
            let blob = block
                .blob_sidecars
                .iter()
                .find(|(h, _)| h == hash)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| {
                    BlobProviderError::Backend(format!(
                        "blob {hash} not found in block {}",
                        block_ref.number
                    ))
                })?;
            blobs.push(blob);
        }
        Ok(blobs)
    }
}

/// In-memory data availability source that reads blob sidecars from
/// [`SharedL1Chain`].
///
/// Implements [`DataAvailabilityProvider`] for action tests that submit batch
/// data as EIP-4844 blobs rather than calldata. Each blob is decoded via
/// [`decode_blob`] before being emitted so the downstream [`FrameQueue`]
/// receives the same format it gets from the real [`BlobSource`] pipeline.
///
/// This source also reads calldata `batcher_txs` so a single source can serve
/// tests that mix calldata and blob submissions.
///
/// [`FrameQueue`]: base_consensus_derive::FrameQueue
/// [`BlobSource`]: base_consensus_derive::BlobSource
#[derive(Debug, Clone)]
pub struct ActionBlobDataSource {
    chain: SharedL1Chain,
    /// The expected batch inbox address.
    inbox_address: Address,
    /// Queued payload bytes for the current block (both calldata and blob data).
    pending: VecDeque<Bytes>,
    /// Whether the current block's data has been loaded.
    open: bool,
}

impl ActionBlobDataSource {
    /// Create a new blob data source backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain, inbox_address: Address) -> Self {
        Self { chain, inbox_address, pending: VecDeque::new(), open: false }
    }

    fn load_block(&mut self, block_ref: &BlockInfo, batcher_address: Address) {
        let Some(block) = self.chain.get_block(block_ref.number) else {
            self.open = true;
            return;
        };
        // Guard against stale block_refs after a reorg.
        if block.hash() != block_ref.hash {
            self.open = true;
            return;
        }
        // Calldata path.
        for tx in &block.batcher_txs {
            if tx.from == batcher_address && tx.to == self.inbox_address {
                self.pending.push_back(tx.input.clone());
            }
        }
        // Blob path: decode the blob and emit the frame bytes.
        for (_, blob) in &block.blob_sidecars {
            self.pending.push_back(decode_blob(blob));
        }
        self.open = true;
    }
}

#[async_trait]
impl DataAvailabilityProvider for ActionBlobDataSource {
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        if !self.open {
            self.load_block(block_ref, batcher_address);
        }
        self.pending.pop_front().ok_or_else(|| PipelineError::Eof.temp())
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.open = false;
    }
}
