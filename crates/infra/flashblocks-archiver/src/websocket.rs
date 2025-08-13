use crate::{cli::BuilderConfig, FlashblockMessage, Metadata};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use rollup_boost::FlashblocksPayloadV1;
use std::io::Read;
use std::time::Duration;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

pub struct WebSocketManager {
    config: BuilderConfig,
}

impl WebSocketManager {
    pub fn new(config: BuilderConfig) -> Self {
        Self { config }
    }

    pub async fn start(
        &self,
        sender: tokio::sync::mpsc::UnboundedSender<(String, FlashblockMessage)>,
    ) -> Result<()> {
        let mut reconnect_interval =
            interval(Duration::from_secs(self.config.reconnect_delay_seconds));

        loop {
            match self.connect_and_listen(&sender).await {
                Ok(_) => {
                    info!(message = "WebSocket connection ended normally", builder_name = %self.config.name);
                }
                Err(e) => {
                    error!(message = "WebSocket connection failed", builder_name = %self.config.name, error = %e);
                }
            }

            info!(message = "Reconnecting to builder", builder_name = %self.config.name, delay_seconds = self.config.reconnect_delay_seconds);
            reconnect_interval.tick().await;
        }
    }

    async fn connect_and_listen(
        &self,
        sender: &tokio::sync::mpsc::UnboundedSender<(String, FlashblockMessage)>,
    ) -> Result<()> {
        info!(message = "Connecting to builder", builder_name = %self.config.name, url = %self.config.url);

        let (ws_stream, _) = connect_async(self.config.url.as_str())
            .await
            .map_err(|e| anyhow!("Failed to connect to {}: {}", self.config.url, e))?;

        info!(message = "Connected to builder", builder_name = %self.config.name);

        let (_, mut read) = ws_stream.split();

        // Listen for messages
        while let Some(message) = read.next().await {
            match message {
                Ok(Message::Binary(data)) => match self.try_decode_message(&data) {
                    Ok(payload) => {
                        if let Err(e) = sender.send((self.config.name.clone(), payload)) {
                            error!(message = "Failed to send parsed binary message to channel", error = %e);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(message = "Failed to decompress/parse binary message", builder_name = %self.config.name, error = %e);
                    }
                },
                Err(e) => {
                    error!(message = "WebSocket error", builder_name = %self.config.name, error = %e);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    pub fn try_decode_message(&self, bytes: &[u8]) -> Result<FlashblockMessage> {
        let text = self.try_parse_message(bytes)?;

        let payload: FlashblocksPayloadV1 = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                return Err(anyhow!("failed to parse message: {}", e));
            }
        };

        let metadata: Metadata = match serde_json::from_value(payload.metadata.clone()) {
            Ok(m) => m,
            Err(e) => {
                return Err(anyhow!("failed to parse message metadata: {}", e));
            }
        };

        Ok(FlashblockMessage {
            payload_id: payload.payload_id,
            index: payload.index,
            base: payload.base,
            diff: payload.diff,
            metadata,
        })
    }

    fn try_parse_message(&self, bytes: &[u8]) -> Result<String> {
        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
            if text.trim_start().starts_with("{") {
                return Ok(text);
            }
        }

        let mut decompressor = brotli::Decompressor::new(bytes, 4096);
        let mut decompressed = Vec::new();
        decompressor.read_to_end(&mut decompressed)?;

        let text = String::from_utf8(decompressed)?;
        Ok(text)
    }
}

pub struct WebSocketPool {
    managers: Vec<WebSocketManager>,
}

impl WebSocketPool {
    pub fn new(builder_configs: Vec<BuilderConfig>) -> Self {
        let managers = builder_configs
            .into_iter()
            .map(WebSocketManager::new)
            .collect();

        Self { managers }
    }

    pub async fn start(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<(String, FlashblockMessage)>> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

        for manager in &self.managers {
            let manager_sender = sender.clone();
            let manager_clone = WebSocketManager::new(manager.config.clone());

            tokio::spawn(async move {
                if let Err(e) = manager_clone.start(manager_sender).await {
                    error!(message = "WebSocket manager failed", error = %e);
                }
            });
        }

        // Drop the original sender so receiver will close when all tasks are done
        drop(sender);

        info!(message = "Started WebSocket connections", connection_count = self.managers.len());
        Ok(receiver)
    }
}
