//! A retry trace event provides a deterministic checkpoint inside retry backoff.

use std::{sync::Arc, time::Duration};

use axum::{Router, extract::State, http::StatusCode, routing::post};
use base_replay::ReplayRecord;
use base_replay_driver::{BuilderIngressInjector, InjectionSchedule};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
};

#[derive(Debug)]
struct RetryBackoff(Arc<Notify>);

impl<S: Subscriber> Layer<S> for RetryBackoff {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().fields().field("backoff_ms").is_some() {
            self.0.notify_one();
        }
    }
}

#[tokio::test]
async fn cancellation_during_backoff_stops_delivery_without_dropping_injector() -> eyre::Result<()>
{
    let backoff = Arc::new(Notify::new());
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(RetryBackoff(Arc::clone(&backoff))),
    )?;
    let (requests, mut received) = mpsc::channel(4);
    let app = Router::new()
        .route(
            "/",
            post(|State(requests): State<mpsc::Sender<()>>| async move {
                requests.send(()).await.unwrap();
                StatusCode::SERVICE_UNAVAILABLE
            }),
        )
        .with_state(requests);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?).parse()?;
    let mut server = JoinSet::new();
    server.spawn(async move { axum::serve(listener, app).await });
    let executor = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()?;
    let injector = BuilderIngressInjector::new(url, &executor)?;
    let record: ReplayRecord = serde_json::from_str(
        r#"{"stream":"builder_ingress","received_at_ms":0,"method":"base_insertValidatedTransaction","params":[],"source":"test"}"#,
    )?;
    let mut schedule = InjectionSchedule::new(vec![record]);
    let cancel = CancellationToken::new();
    let run = schedule.run(&injector, &cancel);
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => panic!("delivery ended before retry backoff: {result:?}"),
        checkpoint = tokio::time::timeout(Duration::from_secs(5), backoff.notified()) => checkpoint?,
    }
    received.try_recv()?;

    cancel.cancel();
    assert!(run.await.is_err());

    assert!(
        tokio::time::timeout(Duration::from_secs(1), received.recv()).await.is_err(),
        "cancelled delivery retried while injector remained alive"
    );
    println!(
        "delivery cancellation: requests_before_cancel=1, requests_after_cancel=0, injector_retained=true"
    );
    Ok(())
}
