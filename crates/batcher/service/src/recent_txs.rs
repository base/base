//! Startup scan of recent L1 blocks for submitted batcher frames.

use std::collections::HashMap;

use alloy_consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable};
use alloy_primitives::Address;
use base_common_genesis::RollupConfig;
use base_common_network::{L1RpcProvider, L1RpcProviderError};
use base_protocol::{Batch, BatchReader, BlockInfo, Channel, ChannelId, Frame};
use futures::StreamExt;
use tracing::{debug, info};

/// Maximum depth allowed for the recent-transaction startup scan.
///
/// Matches the limit used by the reference batcher's `--check-recent-txs-depth` flag.
pub const MAX_CHECK_RECENT_TXS_DEPTH: u64 = 128;

/// Maximum number of L1 block fetches in flight during the startup scan.
///
/// Bounds peak memory to roughly this many full L1 blocks while still
/// achieving significant speedup over sequential fetching.
pub const SCAN_FETCH_CONCURRENCY: usize = 16;

#[derive(Debug)]
struct RecentL1Block {
    info: BlockInfo,
    transactions: Vec<TxEnvelope>,
}

/// Scans recent L1 blocks on startup to find the highest submitted L2 block.
///
/// When the batcher restarts after an unclean shutdown, in-memory channel state
/// is lost. `RecentTxScanner` compensates by reading the last N L1 blocks and
/// decoding any calldata batcher frames sent from the batcher address to the
/// batch inbox. Complete channels are decoded to determine the highest L2 block
/// number already submitted but not yet reflected in the safe head, allowing
/// the block cursor to be advanced accordingly and preventing re-submissions.
#[derive(Debug)]
pub struct RecentTxScanner;

impl RecentTxScanner {
    /// Scans the last `depth` L1 blocks for batcher transactions and returns
    /// the highest L2 block number covered, or `None` if no complete batcher
    /// channels were found.
    ///
    /// Only calldata transactions are decoded (those beginning with
    /// `DERIVATION_VERSION_0`). Blob transactions are identified by their
    /// empty calldata and skipped — their frame data resides in KZG sidecars
    /// that would require a separate fetch not supported by this scanner.
    ///
    /// **Limitation:** channels whose opening frame falls before the scan window
    /// are never completed and will be silently missed. The caller should treat
    /// the result as a best-effort lower bound, not a guarantee.
    pub async fn highest_submitted_l2_block(
        l1_provider: &L1RpcProvider,
        batcher_address: Address,
        batch_inbox: Address,
        depth: u64,
        rollup_config: &RollupConfig,
    ) -> eyre::Result<Option<u64>> {
        let current_l1 = l1_provider
            .get_block_number()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L1 head for recent tx scan: {e}"))?;
        let scan_start = current_l1.saturating_sub(depth.saturating_sub(1));

        info!(
            depth = %depth,
            scan_start = %scan_start,
            scan_end = %current_l1,
            batcher = %batcher_address,
            inbox = %batch_inbox,
            "scanning recent L1 blocks for submitted batcher frames"
        );

        let mut channels: HashMap<ChannelId, Channel> = HashMap::new();
        let mut highest_l2: Option<u64> = None;

        // Fetch blocks in parallel with bounded concurrency, preserving L1 order.
        // Blocks are processed as the stream yields them so peak memory is
        // bounded by the concurrency limit (~16 blocks) rather than the full
        // scan depth (~128 blocks).
        let block_stream = futures::stream::iter(scan_start..=current_l1)
            .map(|block_num| {
                let provider = l1_provider.clone();
                Self::fetch_recent_l1_block(provider, block_num)
            })
            .buffered(SCAN_FETCH_CONCURRENCY);
        futures::pin_mut!(block_stream);

        while let Some(result) = block_stream.next().await {
            let (block_num, block) = result?;
            let block = match block {
                Some(b) => b,
                None => {
                    debug!(block = %block_num, "L1 block not found during recent tx scan");
                    continue;
                }
            };

            for tx in block.transactions {
                if tx.to() != Some(batch_inbox) {
                    continue;
                }
                if tx.recover_signer().ok() != Some(batcher_address) {
                    continue;
                }

                // Only parse calldata (version-0) frames. Blob transactions have
                // empty or absent calldata and will fail parse_frames gracefully.
                let frames = match Frame::parse_frames(tx.input()) {
                    Ok(f) => f,
                    Err(_) => continue,
                };

                for frame in frames {
                    let channel = channels
                        .entry(frame.id)
                        .or_insert_with(|| Channel::new(frame.id, block.info));
                    if let Err(e) = channel.add_frame(frame, block.info) {
                        debug!(error = %e, "ignoring rejected batcher frame during recent tx scan");
                    }
                }
            }

            // Drain channels that became complete within this block.
            let complete_ids: Vec<ChannelId> =
                channels.iter().filter(|(_, ch)| ch.is_ready()).map(|(id, _)| *id).collect();
            for id in complete_ids {
                if let Some(ch) = channels.remove(&id) {
                    Self::decode_channel(&ch, block.info.timestamp, rollup_config, &mut highest_l2);
                }
            }
        }

        if let Some(block) = highest_l2 {
            info!(highest_l2 = %block, "recent tx scan found highest submitted L2 block");
        } else {
            info!("recent tx scan found no submitted batcher frames");
        }

        Ok(highest_l2)
    }

    async fn fetch_recent_l1_block(
        provider: L1RpcProvider,
        block_num: u64,
    ) -> eyre::Result<(u64, Option<RecentL1Block>)> {
        let block = match provider.header_and_transactions_by_number(block_num).await {
            Ok((header, transactions)) => {
                let info = BlockInfo {
                    hash: header.hash_slow(),
                    number: block_num,
                    parent_hash: header.parent_hash,
                    timestamp: header.timestamp,
                };
                Some(RecentL1Block { info, transactions })
            }
            Err(L1RpcProviderError::BlockNotFound(_)) => None,
            Err(e) => return Err(eyre::eyre!("failed to fetch L1 block {block_num}: {e}")),
        };

        Ok((block_num, block))
    }

    /// Decodes all batches from a complete channel and updates `highest_l2` with
    /// the maximum L2 block number found.
    fn decode_channel(
        channel: &Channel,
        inclusion_timestamp: u64,
        rollup_config: &RollupConfig,
        highest_l2: &mut Option<u64>,
    ) {
        let Some(data) = channel.frame_data() else { return };
        let max_rlp = rollup_config.max_rlp_bytes_per_channel(inclusion_timestamp) as usize;
        let brotli_supported = rollup_config.is_fjord_active(inclusion_timestamp);
        let mut reader = BatchReader::new(data.to_vec(), max_rlp, brotli_supported);
        while let Some(batch) = reader.next_batch(rollup_config) {
            let last_timestamp = match &batch {
                Batch::Single(sb) => sb.timestamp,
                Batch::Span(sb) => sb.final_timestamp(),
            };
            let relative = rollup_config.block_number_from_timestamp(last_timestamp);
            let l2_block = rollup_config.genesis.l2.number + relative;
            *highest_l2 = Some(highest_l2.map_or(l2_block, |h| h.max(l2_block)));
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{
        SignableTransaction, TxEip1559, TxEnvelope, transaction::Recovered,
        transaction::TransactionInfo,
    };
    use alloy_eips::eip1898::BlockNumHash;
    use alloy_primitives::{Address, B256, TxKind, U256};
    use alloy_provider::RootProvider;
    use alloy_rlp::Encodable;
    use alloy_rpc_types_eth::Transaction as RpcTransaction;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_genesis::{ChainGenesis, RollupConfig};
    use base_common_network::{Base, L1RpcProvider};
    use base_protocol::{
        Batch, BlockInfo, Channel, ChannelId, DERIVATION_VERSION_0, Frame, SingleBatch,
    };
    use httpmock::{HttpMockRequest, HttpMockResponse, Method::POST, MockServer};
    use serde_json::{Value, json};

    use super::RecentTxScanner;

    /// Build a [`RollupConfig`] with controllable genesis parameters for tests.
    fn test_rollup_config(
        genesis_l2_number: u64,
        genesis_l2_time: u64,
        block_time: u64,
    ) -> RollupConfig {
        RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { number: genesis_l2_number, hash: B256::ZERO },
                l2_time: genesis_l2_time,
                ..Default::default()
            },
            block_time,
            ..Default::default()
        }
    }

    /// Encode a `SingleBatch` into the zlib-compressed channel frame data format
    /// that `BatchReader` expects:
    ///   `zlib_compress`( `rlp_bytes`( `batch_type_byte` ++ `rlp_encode(SingleBatch)` ) )
    fn encode_single_batch(batch: &SingleBatch) -> Vec<u8> {
        // Batch-level encoding: type byte + RLP body.
        let typed_batch = Batch::Single(batch.clone());
        let mut batch_bytes = Vec::new();
        typed_batch.encode(&mut batch_bytes).expect("batch must encode");

        // Wrap as RLP byte string (how ChannelOut wraps it before compressing).
        let mut rlp_buf = Vec::new();
        batch_bytes.as_slice().encode(&mut rlp_buf);

        // Compress with zlib (produces a stream whose first byte has lower nibble 0x8,
        // matching the ZLIB_DEFLATE_COMPRESSION_METHOD check in BatchReader::decompress).
        miniz_oxide::deflate::compress_to_vec_zlib(&rlp_buf, 6)
    }

    /// Create a single-frame channel whose frame data is `payload`.
    fn single_frame_channel(id: ChannelId, payload: Vec<u8>) -> Channel {
        let block_info = BlockInfo::default();
        let mut channel = Channel::new(id, block_info);
        let frame = Frame { id, number: 0, data: payload, is_last: true };
        channel.add_frame(frame, block_info).expect("frame must be accepted");
        channel
    }

    fn json_rpc_response(req: &HttpMockRequest, result: Value) -> String {
        let id = serde_json::from_slice::<Value>(&req.body_vec())
            .ok()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(Value::Null);
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
    }

    fn mock_rpc(server: &MockServer, method: &'static str, result: Value) {
        server.mock(move |when, then| {
            when.method(POST).path("/").json_body_includes(format!(r#"{{"method":"{method}"}}"#));
            then.respond_with(move |req| {
                HttpMockResponse::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(json_rpc_response(req, result.clone()))
                    .build()
            });
        });
    }

    fn deposit_tx_json() -> Value {
        json!({
            "type": "0x7e",
            "hash": "0x096c03d72acb06339c9c7860d1c36b6451932ec0ff16fd34aa9e30a73a245e13",
            "nonce": "0x0",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "transactionIndex": "0x0",
            "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
            "to": "0x4200000000000000000000000000000000000015",
            "value": "0x0",
            "gasPrice": "0x0",
            "gas": "0xf4240",
            "input": "0x",
            "v": "0x0",
            "r": "0x0",
            "s": "0x0",
            "sourceHash": "0x990d7122a1f121f3a6bc45723e28f4921c269037a77e77ffee3c8585136d1a92",
            "mint": "0x0",
            "depositReceiptVersion": "0x1"
        })
    }

    fn eip8130_tx_json() -> Value {
        json!({
            "type": "0x7d",
            "hash": "0x4242424242424242424242424242424242424242424242424242424242424242",
            "blockHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "blockNumber": "0x2a",
            "transactionIndex": "0x1",
            "from": "0x0000000000000000000000000000000000000011",
            "gasPrice": "0x12a05f200",
            "tx": {
                "chainId": 8453,
                "sender": "0x0000000000000000000000000000000000000011",
                "nonceKey": "0x0",
                "nonceSequence": 7,
                "expiry": 0,
                "maxPriorityFeePerGas": "0x3b9aca00",
                "maxFeePerGas": "0x12a05f200",
                "gasLimit": 1_000_000,
                "accountChanges": [],
                "calls": [],
                "payer": null
            },
            "senderAuth": format!("0x{}", "ab".repeat(32)),
            "payerAuth": "0x"
        })
    }

    fn block_with_txs(txs: Vec<Value>) -> Value {
        json!({
            "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "parentHash": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0x3333333333333333333333333333333333333333333333333333333333333333",
            "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0x2a",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x3f2",
            "extraData": "0x",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "transactions": txs,
            "uncles": [],
            "withdrawals": [],
            "blobGasUsed": "0x0",
            "excessBlobGas": "0x0"
        })
    }

    fn frame_input_for_l2_timestamp(timestamp: u64) -> Vec<u8> {
        let batch = SingleBatch { timestamp, ..Default::default() };
        let frame = Frame::new([9u8; 16], 0, encode_single_batch(&batch), true);
        let mut input = vec![DERIVATION_VERSION_0];
        input.extend_from_slice(&frame.encode());
        input
    }

    fn signed_batcher_tx_json(
        signer: &PrivateKeySigner,
        to: Address,
        input: Vec<u8>,
        index: u64,
    ) -> Value {
        let tx = TxEip1559 {
            chain_id: 8453,
            nonce: index,
            gas_limit: 1_000_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: input.into(),
            access_list: Default::default(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("tx signs");
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        let rpc_tx = RpcTransaction::from_transaction(
            Recovered::new_unchecked(envelope, signer.address()),
            TransactionInfo {
                hash: None,
                index: Some(index),
                block_hash: Some(B256::repeat_byte(0x11)),
                block_number: Some(0x2a),
                base_fee: Some(1),
                block_timestamp: Some(0x3f2),
            },
        );
        serde_json::to_value(rpc_tx).expect("transaction serializes")
    }

    async fn run_recent_scan(provider: L1RpcProvider, batcher: Address, inbox: Address) -> u64 {
        let cfg = test_rollup_config(1000, 1000, 2);
        RecentTxScanner::highest_submitted_l2_block(&provider, batcher, inbox, 1, &cfg)
            .await
            .expect("scan succeeds")
            .expect("frame found")
    }

    #[tokio::test]
    async fn highest_submitted_l2_block_base_block_drops_non_standard_txs() {
        let server = MockServer::start_async().await;
        let signer = PrivateKeySigner::from_slice(&[1u8; 32]).expect("valid signer");
        let inbox = Address::repeat_byte(0x88);
        let tx = signed_batcher_tx_json(&signer, inbox, frame_input_for_l2_timestamp(1010), 2);
        mock_rpc(&server, "eth_blockNumber", json!("0x2a"));
        mock_rpc(
            &server,
            "eth_getBlockByNumber",
            block_with_txs(vec![deposit_tx_json(), eip8130_tx_json(), tx]),
        );

        let url = server.url("/").parse().expect("valid url");
        let provider = L1RpcProvider::Base(RootProvider::<Base>::new_http(url));
        let highest = run_recent_scan(provider, signer.address(), inbox).await;

        assert_eq!(highest, 1005);
    }

    #[tokio::test]
    async fn highest_submitted_l2_block_ethereum_path_recovers_frame() {
        let server = MockServer::start_async().await;
        let signer = PrivateKeySigner::from_slice(&[1u8; 32]).expect("valid signer");
        let inbox = Address::repeat_byte(0x88);
        let tx = signed_batcher_tx_json(&signer, inbox, frame_input_for_l2_timestamp(1012), 0);
        mock_rpc(&server, "eth_blockNumber", json!("0x2a"));
        mock_rpc(&server, "eth_getBlockByNumber", block_with_txs(vec![tx]));

        let url = server.url("/").parse().expect("valid url");
        let provider = L1RpcProvider::Ethereum(RootProvider::new_http(url));
        let highest = run_recent_scan(provider, signer.address(), inbox).await;

        assert_eq!(highest, 1006);
    }

    // ── decode_channel tests ─────────────────────────────────────────────────

    /// A channel with no frame data (empty, non-ready channel) must produce no
    /// output and not panic.
    #[test]
    fn decode_channel_no_frame_data_is_noop() {
        let cfg = test_rollup_config(1000, 1000, 2);
        // A channel with no frames has frame_data() == None.
        let channel = Channel::new([0u8; 16], BlockInfo::default());
        let mut highest = None;
        RecentTxScanner::decode_channel(&channel, 0, &cfg, &mut highest);
        assert_eq!(highest, None);
    }

    /// A channel containing one `SingleBatch` with a known timestamp must yield
    /// the correct L2 block number.
    #[test]
    fn decode_channel_single_batch_computes_correct_l2_block() {
        // genesis at L2 block 1000, timestamp 1000, 2-second blocks.
        // batch timestamp 1010 → relative block 5 → L2 block 1005.
        let cfg = test_rollup_config(1000, 1000, 2);
        let batch = SingleBatch { timestamp: 1010, ..Default::default() };

        let id: ChannelId = [1u8; 16];
        let channel = single_frame_channel(id, encode_single_batch(&batch));

        let mut highest = None;
        RecentTxScanner::decode_channel(&channel, 0, &cfg, &mut highest);
        assert_eq!(highest, Some(1005));
    }

    /// When the channel contains multiple batches, `decode_channel` must track
    /// the maximum L2 block across all of them.
    #[test]
    fn decode_channel_multiple_batches_returns_highest() {
        let cfg = test_rollup_config(1000, 1000, 2);

        // Encode two batches into the same compressed payload.
        // batch A: timestamp 1010 → block 1005
        // batch B: timestamp 1020 → block 1010
        let batch_a = SingleBatch { timestamp: 1010, ..Default::default() };
        let batch_b = SingleBatch { timestamp: 1020, ..Default::default() };

        // Encode both into a single byte stream the way ChannelOut would:
        // rlp_bytes(batchA) ++ rlp_bytes(batchB), then zlib-compress.
        let mut combined = Vec::new();
        let mut a_encoded = Vec::new();
        Batch::Single(batch_a).encode(&mut a_encoded).unwrap();
        a_encoded.as_slice().encode(&mut combined);

        let mut b_encoded = Vec::new();
        Batch::Single(batch_b).encode(&mut b_encoded).unwrap();
        b_encoded.as_slice().encode(&mut combined);

        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&combined, 6);

        let id: ChannelId = [2u8; 16];
        let channel = single_frame_channel(id, compressed);

        let mut highest = None;
        RecentTxScanner::decode_channel(&channel, 0, &cfg, &mut highest);
        assert_eq!(highest, Some(1010));
    }

    /// `decode_channel` must not update `highest_l2` when the channel data is
    /// corrupted and `BatchReader` fails to produce any batches.
    #[test]
    fn decode_channel_corrupted_data_is_silently_skipped() {
        let cfg = test_rollup_config(1000, 1000, 2);

        // Craft a payload whose first byte looks like zlib (0x78) but whose body
        // is garbage, so decompression fails and next_batch returns None.
        let junk = vec![0x78u8, 0x9c, 0xde, 0xad, 0xbe, 0xef];

        let id: ChannelId = [3u8; 16];
        let channel = single_frame_channel(id, junk);

        let mut highest = Some(42);
        RecentTxScanner::decode_channel(&channel, 0, &cfg, &mut highest);
        // The existing value must be preserved — no panics, no reset.
        assert_eq!(highest, Some(42));
    }

    /// `decode_channel` must not lower an existing `highest_l2` value: when a
    /// channel yields a block number below the current maximum, the maximum wins.
    #[test]
    fn decode_channel_does_not_lower_existing_highest() {
        let cfg = test_rollup_config(1000, 1000, 2);
        let batch = SingleBatch { timestamp: 1010, ..Default::default() };

        let id: ChannelId = [4u8; 16];
        let channel = single_frame_channel(id, encode_single_batch(&batch));

        // Pre-seed with a higher block number (2000 > 1005).
        let mut highest = Some(2000u64);
        RecentTxScanner::decode_channel(&channel, 0, &cfg, &mut highest);
        assert_eq!(highest, Some(2000));
    }
}
