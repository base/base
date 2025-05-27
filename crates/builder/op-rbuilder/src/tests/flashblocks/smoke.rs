use std::sync::Arc;

use futures::StreamExt;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::tests::TestHarnessBuilder;

#[tokio::test]
#[ignore = "Flashblocks tests need more work"]
async fn chain_produces_blocks() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("flashbots_chain_produces_blocks")
        .with_flashblocks_port(1239)
        .with_chain_block_time(2000)
        .with_flashbots_block_time(200)
        .build()
        .await?;

    // Create a struct to hold received messages
    let received_messages = Arc::new(Mutex::new(Vec::new()));
    let messages_clone = received_messages.clone();
    let cancellation_token = CancellationToken::new();

    // Spawn WebSocket listener task
    let cancellation_token_clone = cancellation_token.clone();
    let ws_handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
        let (ws_stream, _) = connect_async("ws://localhost:1239").await?;
        let (_, mut read) = ws_stream.split();

        loop {
            tokio::select! {
              _ = cancellation_token_clone.cancelled() => {
                  break Ok(());
              }
              Some(Ok(Message::Text(text))) = read.next() => {
                messages_clone.lock().push(text);
              }
            }
        }
    });

    let mut generator = harness.block_generator().await?;

    for _ in 0..10 {
        for _ in 0..5 {
            // send a valid transaction
            let _ = harness.send_valid_transaction().await?;
        }

        generator.generate_block().await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    cancellation_token.cancel();
    assert!(ws_handle.await.is_ok(), "WebSocket listener task failed");
    assert!(
        !received_messages
            .lock()
            .iter()
            .any(|msg| msg.contains("Building flashblock")),
        "No messages received from WebSocket"
    );

    Ok(())
}
