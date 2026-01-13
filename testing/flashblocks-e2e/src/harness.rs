//! Flashblocks streaming and test harness.

use std::{collections::HashSet, io::Read as _, time::Duration};

use alloy_primitives::B256;
use eyre::{Result, WrapErr};
use tokio_tungstenite::tungstenite::Message;

use crate::{TestClient, types::Flashblock};

/// Decode a WebSocket message, handling both text and brotli-compressed binary.
///
/// The flashblocks endpoint sends brotli-compressed binary messages for efficiency.
pub fn decode_ws_message(msg: &Message) -> Result<String> {
    match msg {
        Message::Text(text) => Ok(text.to_string()),
        Message::Binary(data) => {
            // Try brotli decompression
            let mut decompressor = brotli::Decompressor::new(&data[..], 4096);
            let mut decompressed = Vec::new();
            decompressor
                .read_to_end(&mut decompressed)
                .wrap_err("Failed to decompress brotli data")?;

            String::from_utf8(decompressed).wrap_err("Decompressed data is not valid UTF-8")
        }
        Message::Ping(_) | Message::Pong(_) => Err(eyre::eyre!("Unexpected ping/pong message")),
        Message::Close(_) => Err(eyre::eyre!("WebSocket closed")),
        Message::Frame(_) => Err(eyre::eyre!("Unexpected raw frame")),
    }
}

/// Active WebSocket subscription.
pub struct WebSocketSubscription {
    /// The underlying WebSocket stream.
    pub stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// The subscription ID.
    pub subscription_id: String,
}

impl std::fmt::Debug for WebSocketSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketSubscription")
            .field("subscription_id", &self.subscription_id)
            .finish_non_exhaustive()
    }
}

impl WebSocketSubscription {
    /// Receive the next notification.
    ///
    /// Handles both text messages and brotli-compressed binary messages
    /// (the flashblocks endpoint uses brotli compression).
    pub async fn next_notification(&mut self) -> Result<serde_json::Value> {
        use futures_util::StreamExt;

        let msg = self
            .stream
            .next()
            .await
            .ok_or_else(|| eyre::eyre!("WebSocket closed"))?
            .wrap_err("Failed to receive message")?;

        let json_str = decode_ws_message(&msg)?;

        let notification: serde_json::Value =
            serde_json::from_str(&json_str).wrap_err("Failed to parse notification")?;

        Ok(notification)
    }

    /// Unsubscribe and close the connection.
    pub async fn unsubscribe(mut self) -> Result<()> {
        use futures_util::SinkExt;

        let unsubscribe_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "eth_unsubscribe",
            "params": [self.subscription_id]
        });

        self.stream
            .send(Message::Text(unsubscribe_msg.to_string().into()))
            .await
            .wrap_err("Failed to send unsubscribe")?;

        Ok(())
    }
}

/// Direct streaming connection to the flashblocks WebSocket endpoint.
///
/// Unlike `WebSocketSubscription`, this doesn't use eth_subscribe - it just
/// connects and immediately starts receiving flashblock messages.
pub struct FlashblocksStream {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl std::fmt::Debug for FlashblocksStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashblocksStream").finish_non_exhaustive()
    }
}

impl FlashblocksStream {
    /// Connect to the flashblocks WebSocket endpoint.
    pub async fn connect(url: &str) -> Result<Self> {
        use tokio_tungstenite::connect_async;

        let (ws_stream, _) =
            connect_async(url).await.wrap_err("Failed to connect to flashblocks WebSocket")?;

        Ok(Self { stream: ws_stream })
    }

    /// Receive the next flashblock message.
    pub async fn next_flashblock(&mut self) -> Result<Flashblock> {
        use futures_util::StreamExt;

        loop {
            let msg = self
                .stream
                .next()
                .await
                .ok_or_else(|| eyre::eyre!("Flashblocks WebSocket closed"))?
                .wrap_err("Failed to receive flashblock message")?;

            // Handle ping/pong internally
            if msg.is_ping() || msg.is_pong() {
                continue;
            }

            // Extract bytes from message
            let bytes: Vec<u8> = match msg {
                Message::Text(text) => text.as_bytes().to_vec(),
                Message::Binary(data) => data.to_vec(),
                Message::Close(_) => return Err(eyre::eyre!("WebSocket closed")),
                _ => continue,
            };

            let flashblock = Flashblock::try_decode_message(bytes)
                .map_err(|e| eyre::eyre!("Failed to decode flashblock: {}", e))?;

            return Ok(flashblock);
        }
    }

    /// Close the connection.
    pub async fn close(self) -> Result<()> {
        // Just drop the stream - it will close gracefully
        drop(self.stream);
        Ok(())
    }
}

/// Harness for running tests within a flashblock window.
///
/// This ensures tests can:
/// 1. Wait for flashblock 0/1 of a new block (fresh start)
/// 2. Query pre-state
/// 3. Send transactions
/// 4. See them appear in flashblocks (same block)
/// 5. Query post-state
/// 6. Confirm no flashblocks for the next block were received
pub struct FlashblockHarness {
    stream: FlashblocksStream,
    /// The block number we're operating in.
    block_number: u64,
    /// Count of flashblocks received for current block.
    flashblock_count: u64,
    /// Transaction hashes seen in the current block (from all flashblocks).
    seen_tx_hashes: HashSet<B256>,
    /// The most recent flashblock received.
    current_flashblock: Option<Flashblock>,
}

impl std::fmt::Debug for FlashblockHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashblockHarness")
            .field("block_number", &self.block_number)
            .field("flashblock_count", &self.flashblock_count)
            .field("seen_tx_hashes", &self.seen_tx_hashes.len())
            .finish_non_exhaustive()
    }
}

impl FlashblockHarness {
    /// Create a new harness and wait for the start of a fresh block.
    ///
    /// Connects to the flashblocks WebSocket and waits until we see flashblock
    /// index 0 or 1 of a new block, ensuring we have a full block window to work with.
    /// This is critical because tests need to send transactions and see them in
    /// pending state BEFORE the block is committed.
    pub async fn new(client: &TestClient) -> Result<Self> {
        let stream = FlashblocksStream::connect(&client.flashblocks_ws_url).await?;

        tracing::debug!("Connected to flashblocks stream");

        let mut harness = Self {
            stream,
            block_number: 0,
            flashblock_count: 0,
            seen_tx_hashes: HashSet::new(),
            current_flashblock: None,
        };

        // Wait for the start of a fresh block (index 0 or 1)
        // This ensures we have most of the block window available for our test
        loop {
            let flashblock = harness.wait_for_next_flashblock().await?;
            let fb_index = flashblock.index;
            let fb_block = flashblock.metadata.block_number;

            if fb_index <= 1 {
                tracing::info!(
                    block_number = fb_block,
                    flashblock_index = fb_index,
                    "Harness ready at start of block"
                );
                break;
            }
            tracing::debug!(
                block_number = fb_block,
                flashblock_index = fb_index,
                "Waiting for fresh block start (index 0 or 1)..."
            );
        }

        Ok(harness)
    }

    /// Wait for the next flashblock.
    /// Updates internal state and returns the new flashblock.
    pub async fn wait_for_next_flashblock(&mut self) -> Result<&Flashblock> {
        let flashblock = self.stream.next_flashblock().await?;

        let new_block_number = flashblock.metadata.block_number;

        if new_block_number != self.block_number {
            // New block started - reset tracking
            self.block_number = new_block_number;
            self.flashblock_count = 1;
            self.seen_tx_hashes.clear();
            tracing::debug!(block_number = new_block_number, "New block started");
        } else {
            self.flashblock_count += 1;
        }

        // Track all transaction hashes from this flashblock's receipts
        for tx_hash in flashblock.metadata.receipts.keys() {
            self.seen_tx_hashes.insert(*tx_hash);
        }

        tracing::trace!(
            block_number = self.block_number,
            flashblock_index = flashblock.index,
            flashblock_count = self.flashblock_count,
            tx_count = flashblock.diff.transactions.len(),
            "Received flashblock"
        );

        self.current_flashblock = Some(flashblock);
        Ok(self.current_flashblock.as_ref().unwrap())
    }

    /// Wait for a transaction to appear in a flashblock within the current block.
    ///
    /// This waits for the transaction to appear in a flashblock. If a new block
    /// starts before the transaction is seen, this fails - the test must complete
    /// within a single block window to properly test pending state visibility.
    pub async fn wait_for_tx(&mut self, tx_hash: B256, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let starting_block = self.block_number;

        // Check if we already have the tx
        if self.seen_tx_hashes.contains(&tx_hash) {
            tracing::debug!(
                ?tx_hash,
                block = self.block_number,
                "Transaction already seen in flashblock"
            );
            return Ok(());
        }

        loop {
            // Wait for next flashblock with timeout
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(eyre::eyre!(
                    "Timeout waiting for tx {:?} in flashblock (block {})",
                    tx_hash,
                    starting_block
                ));
            }

            match tokio::time::timeout(remaining, self.stream.next_flashblock()).await {
                Ok(Ok(flashblock)) => {
                    let new_block_number = flashblock.metadata.block_number;

                    if new_block_number != self.block_number {
                        // Block boundary crossed - fail the test
                        return Err(eyre::eyre!(
                            "Block {} ended before tx {:?} appeared in pending state. \
                             New block {} started. This can happen if the RPC node forwards \
                             transactions to a remote sequencer with latency.",
                            starting_block,
                            tx_hash,
                            new_block_number
                        ));
                    }

                    self.flashblock_count += 1;

                    // Track all transaction hashes from this flashblock
                    for hash in flashblock.metadata.receipts.keys() {
                        self.seen_tx_hashes.insert(*hash);
                    }

                    if self.seen_tx_hashes.contains(&tx_hash) {
                        tracing::debug!(
                            ?tx_hash,
                            block = self.block_number,
                            "Transaction found in flashblock"
                        );
                        self.current_flashblock = Some(flashblock);
                        return Ok(());
                    }

                    self.current_flashblock = Some(flashblock);
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(eyre::eyre!(
                        "Timeout waiting for tx {:?} in flashblock (block {})",
                        tx_hash,
                        starting_block
                    ));
                }
            }
        }
    }

    /// Assert that we're still in the same block (no next block flashblocks received).
    ///
    /// This is called after test operations to confirm everything happened
    /// within a single block's flashblock window.
    pub fn assert_same_block(&self, expected_block: u64) -> Result<()> {
        if self.block_number != expected_block {
            return Err(eyre::eyre!(
                "Block changed during test: expected {}, now at {}",
                expected_block,
                self.block_number
            ));
        }
        Ok(())
    }

    /// Get the current block number we're operating in.
    pub const fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Get the count of flashblocks received for the current block.
    pub const fn flashblock_count(&self) -> u64 {
        self.flashblock_count
    }

    /// Close the stream.
    pub async fn close(self) -> Result<()> {
        self.stream.close().await
    }
}
