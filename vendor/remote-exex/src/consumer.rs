use remote_exex::proto::{SubscribeRequest, remote_ex_ex_client::RemoteExExClient};
use reth_exex::ExExNotification;
use reth_tracing::{RethTracer, Tracer, tracing::info};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _ = RethTracer::new().init()?;

    let mut client = RemoteExExClient::connect("http://[::1]:10000")
        .await?
        .max_encoding_message_size(usize::MAX)
        .max_decoding_message_size(usize::MAX);

    let mut stream = client.subscribe(SubscribeRequest {}).await?.into_inner();
    while let Some(notification) = stream.message().await? {
        let notification: ExExNotification = bincode_1_3_3::deserialize(&notification.data)?;

        match notification {
            ExExNotification::ChainCommitted { new } => {
                info!(committed_chain = ?new.range(), "Received commit");
            }
            ExExNotification::ChainReorged { old, new } => {
                info!(from_chain = ?old.range(), to_chain = ?new.range(), "Received reorg");
            }
            ExExNotification::ChainReverted { old } => {
                info!(reverted_chain = ?old.range(), "Received revert");
            }
        };
    }

    Ok(())
}
