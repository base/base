# Batcher

[derivation spec]: consensus/derivation.md

## Overview

The batcher, also referred to as the batch submitter, is the entity responsible for posting L2 sequencer data to L1, making it available to the derivation pipeline operated by verifiers. The format of batcher transactions — channels, frames, and batches within them — is defined in the [derivation spec]: the data is constructed from L2 blocks in the reverse order from which it is derived back into L2 blocks. Only data that conforms to those rules will be accepted as valid from the verifier's perspective.

The batcher observes the gap between the unsafe L2 head (the latest sequenced block) and the safe L2 head (the latest block confirmed on L1 through derivation). Any unsafe L2 blocks that have not yet been confirmed must be encoded and submitted. The batcher encodes L2 blocks into channels, fragments channels into frames, and posts frames as L1 transactions. The derivation pipeline then reads those frames, reassembles channels, decodes batches, and reconstructs the original L2 blocks.

The timing and transaction signing are implementation-specific: data can be submitted at any time, but only data that matches the [derivation spec] rules will be valid from the verifier perspective. The L2 view of safe and unsafe does not update instantly after data is submitted or confirmed on L1, so a batcher implementation must take care not to duplicate data submissions.

## Channel Lifecycle

A channel is the unit of encoding used by the batcher. It is an ordered, compressed sequence of RLP-encoded L2 block batches. A channel is opened when there are L2 blocks awaiting submission and no channel is currently open. At most one channel may be open at any time; a new channel must not be opened until the previous one has been fully closed and all its frames have been submitted to L1.

A channel accumulates L2 block batches in strictly increasing block number order until one of the following closure conditions is met. A channel must close when adding the next batch would cause the compressed output size to exceed the maximum blob data capacity, ensuring that no frame will carry a payload too large for its data availability target. A channel must also close when continued accumulation would cause the total uncompressed RLP byte length of its batches to exceed `max_rlp_bytes_per_channel`, a protocol limit that protects verifiers against decompression amplification. In both cases, the batch that would have caused the overflow is withheld from the current channel; the channel is closed, and that batch becomes the first entry of the next channel.

A channel must additionally close on timeout: if the L1 chain advances more than `max_channel_duration` L1 blocks beyond the block at which the channel was opened, the channel must be closed and its frames posted immediately. This prevents channels from staying open indefinitely and ensures that verifiers — who drop any channel not completed within the `channel_timeout` window — do not discard the data.

When a channel closes, its compressed data is partitioned into fixed-size frames. Each frame carries at most `max_frame_size` bytes of compressed payload plus per-frame header overhead. The resulting frames are queued for submission to L1 in order. The channel's block range — the contiguous interval of L2 block numbers it covers — is fixed upon closing and must not change.

## Frame Production and Ordering

Each frame carries a header identifying the channel it belongs to via a 16-byte channel ID, its position within the channel as a monotonically increasing 16-bit frame number beginning at zero, the length of its compressed payload, and a boolean flag indicating whether it is the last frame in the channel. The first frame of each channel additionally carries a single version byte identifying the compression codec; all subsequent frames consist entirely of compressed payload with no such prefix.

Frames within a channel must be submitted to L1 in sequential order. Frame `N` must appear on L1 no later than frame `N+1`. The derivation pipeline may tolerate out-of-order frame delivery in some configurations, but from the Holocene hardfork onward it drops any non-first frame whose frame number is not exactly one greater than the previous frame received for that channel, and drops any new first frame whose predecessor channel has not yet been closed. After Holocene activation, strict in-order delivery is required for correctness.

The `is_last` flag must be set to true on exactly the final frame of a channel and false on all preceding frames. A verifier considers a channel complete only when a frame with `is_last` set is received. Any channel that never receives its final frame within the `channel_timeout` window is discarded by the verifier.

## Data Availability

The batcher posts frames to L1 as batcher transactions addressed to the batcher inbox address, which is a designated EOA rather than a contract. Each batcher transaction must be signed by the batcher's signing key, and the recovered sender address must match the `batcherAddress` recorded in the L2 system configuration at the time of the L1 transaction's inclusion. The derivation pipeline authenticates batcher transactions by this address; transactions from any other sender are ignored regardless of their content.

As of the Cancun L1 upgrade, the primary data availability mechanism is EIP-4844 blob transactions. Each blob carries one frame of compressed channel data. The maximum usable payload per blob is 130,044 bytes, which defines the effective `max_frame_size`. The batcher must not produce frames whose compressed payload exceeds this limit.

All frames for a given channel must land on L1 within `channel_timeout` L1 blocks of the block in which the channel's first frame was included. If the channel is not completed within this window, the derivation pipeline discards all buffered frames for that channel, and the affected L2 blocks must be resubmitted in a new channel. The batcher must size channels and manage submission throughput to ensure frames are posted within this deadline.

## Block Continuity

The batcher encodes L2 blocks in strictly increasing order by block number. Each block added to the open channel must be the direct child of the previously encoded block: its parent hash must equal the hash of the most recently encoded block. This invariant ensures the channel represents a contiguous, unambiguous segment of the canonical L2 chain.

If the L2 chain reorganizes — manifesting as a block whose parent hash does not match the previously seen tip, or as an explicit reorg signal from the block source — the batcher must discard all pending encoding state. This includes the currently open channel, any channels queued for submission but not yet fully confirmed, and all in-flight submission tracking. After a reorg, the batcher restarts from the new canonical chain tip. L1 transactions already in flight at the time of the reorg are abandoned; if they are eventually included on L1, the derivation pipeline ignores them as they are incoherent with the new chain.

Each channel covers a contiguous, non-overlapping range of L2 block numbers. The block range of a subsequent channel must begin exactly where the block range of the preceding channel ends. No L2 block may appear in more than one channel, and no blocks may be skipped between consecutive channels.

## Sequencer Drift and Throttling

The derivation spec constrains how far the L2 timestamp may advance ahead of the L1 timestamp of its origin block. An L2 block's timestamp must not exceed the L1 origin timestamp plus `max_sequencer_drift`. Prior to the Fjord hardfork, `max_sequencer_drift` is a per-chain configuration parameter. From Fjord onward it is fixed at 1800 seconds. When this limit is exceeded, the derivation pipeline will only accept a batch if its transaction list is empty (a deposit-only block). The batcher must therefore not include user transactions in blocks whose timestamp would exceed the drift limit, and must coordinate with the sequencer accordingly.

To prevent the sequencer from outpacing the batcher's L1 submission capacity, the batcher measures its data availability backlog — the total encoded size of L2 blocks that have been sequenced but whose data has not yet been confirmed on L1. When the backlog exceeds a configured threshold, the batcher signals the sequencer to reduce its block production rate. The throttle can be graduated: a modest backlog may request a modest slowdown, while a large backlog may pause block production entirely until the batcher catches up. This feedback mechanism is transparent to the derivation pipeline and is not reflected in any on-chain data.

## Compression

Channel data is compressed before being partitioned into frames. Prior to the Fjord hardfork, channels use zlib compression (RFC 1950, no dictionary) and carry no version prefix; the zlib magic bytes in the stream allow the decompressor to identify the format. From Fjord onward, channels use Brotli compression (RFC 7932), and the first frame of each channel carries a version byte of `0x01` immediately before the compressed payload to identify the codec. The lower nibble of the version byte must not be `0x08` or `0x0f`, as those values would collide with zlib magic header bytes and confuse earlier decompressors.

Because compression ratios vary with input content, the batcher must estimate the compressed output size prospectively as it encodes batches into a channel. The channel must be closed before the compressed output would exceed `max_frame_size`, rather than after. A common approach is to maintain a shadow compressor in parallel with the real compressor and treat the shadow's output size as an upper bound; the channel is closed when the shadow output reaches the limit. This ensures the batcher never produces a frame too large to fit within a blob.

The maximum uncompressed RLP size per channel, `max_rlp_bytes_per_channel`, is enforced separately from the compressed size limit. This limit protects verifiers from decompression amplification: a small compressed payload that expands to an unboundedly large uncompressed stream could exhaust memory. A verifier decoding a channel stops processing once the uncompressed output reaches this limit; any remaining batches are discarded. The batcher must ensure the uncompressed size of its batches does not exceed this bound, both to guarantee all batches are seen by verifiers and to stay within the protocol's defined limits.

## Confirmation and Block Pruning

The batcher tracks each submitted frame until it is included in an L1 block. A frame is confirmed when the batcher observes an L1 block containing the L1 transaction that carries the frame. A channel is fully confirmed when every one of its frames has been confirmed on L1.

L2 blocks must not be discarded from the batcher's pending set until the channel containing them is fully confirmed. Until confirmation, those blocks must be retained so that any lost frames — for example due to an L1 reorg removing the transaction's inclusion — can be reconstructed and resubmitted. Only after a channel is fully confirmed may the batcher release the L2 blocks it covers.

If a submitted frame's L1 transaction fails to be included, the batcher must resubmit that frame and all subsequent frames in the same channel. Resubmitted frames must be byte-identical to the originals: the derivation pipeline identifies frames by their channel ID and frame number, and a resubmitted frame with different content would be treated as corrupted data rather than as a retry.

## Hardfork Rules

The Fjord hardfork changes the channel encoding format. Channels opened after Fjord activation must use Brotli compression and prefix the first frame's payload with version byte `0x01`. The protocol limit `max_rlp_bytes_per_channel` increases substantially at Fjord activation, relaxing the channel size constraint. Channels opened before Fjord activation must use the pre-Fjord format for all their frames, regardless of when those frames are posted.

The Holocene hardfork imposes strict ordering requirements at both the frame and batch layers. At the frame layer, frames for a given channel must be delivered to the derivation pipeline contiguously and in order; a non-first frame that is not the immediate successor of the previously seen frame for that channel is dropped immediately, and an incomplete channel is dropped if a new first frame for it arrives before its final frame has been seen. At the batch layer, batches within a channel must be strictly ordered by L2 timestamp with no repeated timestamps; any batch with a timestamp not strictly greater than the previous batch in the same channel causes the channel to be invalidated and all remaining batches in it to be dropped. These rules impose no new on-chain obligations, but they mean the batcher has zero tolerance for frame delivery gaps or reordering after Holocene activation.
