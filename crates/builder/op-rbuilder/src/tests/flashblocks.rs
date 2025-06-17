use std::sync::Arc;

use futures::StreamExt;
use macros::rb_test;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::{
    args::{FlashblocksArgs, OpRbuilderArgs},
    tests::{ChainDriverExt, LocalInstance, TransactionBuilderExt},
};

#[rb_test(flashblocks, args = OpRbuilderArgs {
    chain_block_time: 2000,
    flashblocks: FlashblocksArgs {
        enabled: true,
        flashblocks_port: 1239,
        flashblocks_addr: "127.0.0.1".into(),
        flashblocks_block_time: 200,
        flashblocks_leeway_time: 0,
        flashblocks_dynamic: false,
    },
    ..Default::default()
})]
async fn smoke(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;
    driver.fund_default_accounts().await?;

    // Create a struct to hold received messages
    let received_messages = Arc::new(Mutex::new(Vec::new()));
    let messages_clone = received_messages.clone();
    let cancellation_token = CancellationToken::new();
    let flashblocks_ws_url = rbuilder.flashblocks_ws_url();

    // Spawn WebSocket listener task
    let cancellation_token_clone = cancellation_token.clone();
    let ws_handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
        let (ws_stream, _) = connect_async(flashblocks_ws_url).await?;
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

    for _ in 0..10 {
        for _ in 0..5 {
            // send a valid transaction
            let _ = driver
                .create_transaction()
                .random_valid_transfer()
                .send()
                .await?;
        }

        let block = driver.build_new_block().await?;
        assert_eq!(block.transactions.len(), 8); // 5 normal txn + deposit + 2 builder txn

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
