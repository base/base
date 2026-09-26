//! The admin JSON-RPC API end to end: a real [`AdminServer`] over a real `BatchDriver`, called
//! over HTTP with the method names and payloads operators send.

use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};

use base_batcher_admin::AdminServer;
use base_batcher_core::{
    BatchDriverError,
    test_utils::{DriverFixture, DriverHandles, Recorded, ScriptedTxManager, TrackingPipeline},
};
use base_runtime::{Cancellation, TokioRuntime};
use jsonrpsee::{
    core::{ClientError, client::ClientT, params::ArrayParams},
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

/// The DA backlog the driver's pipeline reports, above the threshold of [`throttle_config`].
const DA_BACKLOG_BYTES: u64 = 1_500;

/// A driver running in the background, its admin server, and an HTTP client on it.
struct AdminRpc {
    client: HttpClient,
    runtime: TokioRuntime,
    driver: JoinHandle<Result<(), BatchDriverError>>,
    recorded: Arc<Mutex<Recorded>>,
    _server: AdminServer,
    /// Kept alive: the driver exits once the derivation status sender is dropped.
    _handles: DriverHandles,
}

impl AdminRpc {
    async fn start() -> Self {
        let runtime = TokioRuntime::new();
        let pipeline = TrackingPipeline::new().with_da_backlog(DA_BACKLOG_BYTES);
        let recorded = pipeline.recorded();
        let (driver, handles) =
            DriverFixture::new(runtime.clone(), pipeline, ScriptedTxManager::new([])).build();
        let driver = tokio::spawn(driver.run());
        let server = AdminServer::spawn((Ipv4Addr::LOCALHOST, 0).into(), handles.admin.clone())
            .await
            .expect("the admin server binds");
        let client = HttpClientBuilder::default()
            .build(format!("http://{}", server.local_addr()))
            .expect("the client builds");
        Self { client, runtime, driver, recorded, _server: server, _handles: handles }
    }

    /// The JSON-RPC error code and message the call fails with.
    async fn error(&self, method: &str, params: ArrayParams) -> (i32, String) {
        match self.client.request::<Value, _>(method, params).await {
            Err(ClientError::Call(error)) => (error.code(), error.message().to_string()),
            other => panic!("expected a JSON-RPC error, got {other:?}"),
        }
    }
}

/// A throttle config as an operator sends it, with no value at its default.
fn throttle_config(max_intensity: f64) -> Value {
    json!({
        "threshold_bytes": 1_000,
        "max_intensity": max_intensity,
        "block_size_lower_limit": 3_000,
        "block_size_upper_limit": 100_000,
        "tx_size_lower_limit": 200,
        "tx_size_upper_limit": 10_000,
    })
}

#[tokio::test]
async fn status_reports_the_driver_state_with_the_documented_fields() {
    let rpc = AdminRpc::start().await;

    let status: Value = rpc.client.request("admin_getBatcherStatus", rpc_params![]).await.unwrap();

    assert_eq!(
        status,
        json!({ "stopped": false, "in_flight": 0, "da_backlog_bytes": DA_BACKLOG_BYTES })
    );
}

/// A stop is reported by the status and refuses a flush. A start lets the flush through to
/// the pipeline.
#[tokio::test]
async fn stop_and_start_gate_the_flush() {
    let rpc = AdminRpc::start().await;

    let () = rpc.client.request("admin_stopBatcher", rpc_params![]).await.unwrap();
    let status: Value = rpc.client.request("admin_getBatcherStatus", rpc_params![]).await.unwrap();
    assert_eq!(status["stopped"], json!(true));
    assert_eq!(
        rpc.error("admin_flushBatcher", rpc_params![]).await,
        (-32002, "batcher is stopped".into())
    );

    let () = rpc.client.request("admin_startBatcher", rpc_params![]).await.unwrap();
    let () = rpc.client.request("admin_flushBatcher", rpc_params![]).await.unwrap();
    assert_eq!(rpc.recorded.lock().unwrap().flushes(), 1);
}

/// The throttle controller is read back as set and applied to the backlog, an invalid config
/// is refused as invalid params, and a reset is accepted.
#[tokio::test]
async fn throttle_controller_is_set_read_and_reset() {
    let rpc = AdminRpc::start().await;

    let params = rpc_params!["step", throttle_config(0.5)];
    let () = rpc.client.request("admin_setThrottleController", params).await.unwrap();

    // The backlog is above the threshold, so the step strategy throttles at half intensity:
    // the limits sit halfway between their upper and lower bounds.
    let info: Value =
        rpc.client.request("admin_getThrottleController", rpc_params![]).await.unwrap();
    assert_eq!(
        info,
        json!({
            "strategy": "step",
            "threshold_bytes": 1_000,
            "max_intensity": 0.5,
            "current_intensity": 0.5,
            "max_block_size": 51_500,
            "max_tx_size": 5_100,
        })
    );

    let params = rpc_params!["step", throttle_config(2.0)];
    assert_eq!(
        rpc.error("admin_setThrottleController", params).await,
        (-32602, "invalid throttle config: max_intensity (2) must be within [0, 1]".into())
    );

    let () = rpc.client.request("admin_resetThrottleController", rpc_params![]).await.unwrap();
}

#[tokio::test]
async fn set_log_level_is_not_supported() {
    let rpc = AdminRpc::start().await;

    assert_eq!(
        rpc.error("admin_setLogLevel", rpc_params!["debug"]).await,
        (-32601, "not yet supported: set_log_level".into())
    );
}

/// Once the driver has exited, every command that reaches it fails with the shut-down code.
#[tokio::test]
async fn driver_commands_fail_once_the_driver_has_exited() {
    let mut rpc = AdminRpc::start().await;
    rpc.runtime.cancel();
    (&mut rpc.driver).await.unwrap().unwrap();

    let commands = [
        ("admin_startBatcher", rpc_params![]),
        ("admin_stopBatcher", rpc_params![]),
        ("admin_flushBatcher", rpc_params![]),
        ("admin_getBatcherStatus", rpc_params![]),
        ("admin_getThrottleController", rpc_params![]),
        ("admin_setThrottleController", rpc_params!["step", throttle_config(1.0)]),
        ("admin_resetThrottleController", rpc_params![]),
    ];
    for (method, params) in commands {
        assert_eq!(
            rpc.error(method, params).await,
            (-32001, "admin channel closed: driver has shut down".into()),
            "{method} after the driver exited"
        );
    }
}
