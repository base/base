use std::sync::Arc;

use remote_exex::proto::{
    self,
    remote_ex_ex_server::{RemoteExEx, RemoteExExServer},
};
use reth_exex::ExExNotification;
use reth_node_ethereum::EthereumNode;
use reth_tracing::tracing::info;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic_0_11::{Request, Response, Status, transport::Server};

struct ExExService {
    notifications: Arc<broadcast::Sender<ExExNotification>>,
}

// ANCHOR: snippet
#[tonic_0_11::async_trait]
impl RemoteExEx for ExExService {
    type SubscribeStream = ReceiverStream<Result<proto::ExExNotification, Status>>;

    async fn subscribe(
        &self,
        _request: Request<proto::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let (tx, rx) = mpsc::channel(1);

        let mut notifications = self.notifications.subscribe();
        tokio::spawn(async move {
            while let Ok(notification) = notifications.recv().await {
                let proto_notification = proto::ExExNotification {
                    data: bincode_1_3_3::serialize(&notification).expect("failed to serialize"),
                };
                tx.send(Ok(proto_notification))
                    .await
                    .expect("failed to send notification to client");

                info!("Notification sent to the gRPC client");
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
// ANCHOR_END: snippet

fn main() -> eyre::Result<()> {
    reth::cli::Cli::parse_args().run(async move |builder, _| {
        let notifications = Arc::new(broadcast::channel(1).0);

        let server = Server::builder()
            .add_service(RemoteExExServer::new(ExExService {
                notifications: notifications.clone(),
            }))
            .serve("[::1]:10000".parse().unwrap());

        let handle = builder.node(EthereumNode::default()).launch().await?;

        handle.node.task_executor.spawn_critical("gRPC server", async move {
            server.await.expect("failed to start gRPC server")
        });

        handle.wait_for_node_exit().await
    })
}
