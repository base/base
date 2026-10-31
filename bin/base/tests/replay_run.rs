//! Execute the actual CLI against two real builders and inspect its report artifacts.

use std::time::{Duration, UNIX_EPOCH};

use alloy_provider::Provider;
use base_builder_core::test_utils::LocalInstance;
use base_clock::{Clock, GlobalClock, OffsetClock, SystemClock};
use base_replay_driver::ReplayDataset;
use base_replay_export::{ExportManifest, ExportWindow};
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use serde_json::value::RawValue;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread")]
async fn writes_reports_when_cli_attaches_to_real_builder() -> eyre::Result<()> {
    let canonical = LocalInstance::flashblocks().await?;
    let source = canonical.driver().await?;
    let first = source.build_new_block_with_no_tx_pool().await?;
    let last = source.build_new_block_with_no_tx_pool().await?;
    let from = first.header.timestamp * 1000;
    let epoch = from - 8_000 - ReplayDataset::CLOCK_MARGIN_MS;
    let started = SystemClock.now_unix_millis()?;
    GlobalClock::set(OffsetClock::new(UNIX_EPOCH + Duration::from_millis(epoch)))?;
    let shadow = LocalInstance::flashblocks().await?;
    let mut rpc = RpcModule::new((canonical.provider().await?, shadow.provider().await?));
    for method in ["eth_chainId", "eth_getBlockByNumber", "eth_getBlockReceipts"] {
        rpc.register_async_method(method, move |params, context, _| async move {
            let raw: Box<RawValue> = params.parse()?;
            context
                .0
                .raw_request::<_, Box<RawValue>>(method.into(), raw)
                .await
                .map_err(|error| ErrorObjectOwned::owned(-32000, error.to_string(), None::<()>))
        })?;
    }
    rpc.register_async_method("eth_sendRawTransaction", |params, context, _| async move {
        let raw: Box<RawValue> = params.parse()?;
        context
            .1
            .raw_request::<_, Box<RawValue>>("eth_sendRawTransaction".into(), raw)
            .await
            .map_err(|error| ErrorObjectOwned::owned(-32000, error.to_string(), None::<()>))
    })?;
    let server = Server::builder().build("127.0.0.1:0").await?;
    let rpc_url = format!("http://{}", server.local_addr()?);
    let server = server.start(rpc);
    let dir = tempfile::tempdir()?;
    let export = dir.path().join("export");
    tokio::fs::create_dir(&export).await?;
    let manifest = ExportManifest {
        format: "replay-export/v1".into(),
        record_format: "replay-record/v1".into(),
        network: "test".into(),
        record_count: 0,
        window: ExportWindow {
            chain_id: 901,
            from_block: 1,
            to_block: 2,
            from_timestamp_ms: from,
            to_timestamp_ms: last.header.timestamp * 1000,
            warmup_ms: 8_000,
            start_ms: from - 8_000,
            end_ms: last.header.timestamp * 1000 + 2_000,
        },
    };
    tokio::fs::write(export.join("manifest.json"), serde_json::to_vec(&manifest)?).await?;
    tokio::fs::write(export.join("records.jsonl"), "").await?;
    let chain = dir.path().join("genesis.json");
    tokio::fs::write(&chain, serde_json::to_vec(&shadow.node_config().chain.inner.genesis)?)
        .await?;
    let jwt = dir.path().join("jwt.hex");
    alloy_rpc_types_engine::JwtSecret::try_create_random(&jwt)?;
    let out = dir.path().join("diff");
    let engine_url = url::Url::from_file_path(shadow.auth_ipc()).unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(env!("CARGO_BIN_EXE_base"))
            .kill_on_drop(true)
            .env_remove("BASE_CHAIN")
            .args(["replay", "run", "--export"])
            .arg(&export)
            .arg("--chain")
            .arg(&chain)
            .args([
                "--l2-rpc",
                &rpc_url,
                "--builder-engine",
                engine_url.as_str(),
                "--builder-rpc",
                &rpc_url,
            ])
            .arg("--builder-jwt")
            .arg(&jwt)
            .args([
                "--builder-started-at-ms",
                &started.to_string(),
                "--clock-start-ms",
                &epoch.to_string(),
            ])
            .arg("--out")
            .arg(&out)
            .output(),
    )
    .await??;

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(out.join("diff.json")).await?)?;
    assert_eq!(report["schema"], "replay-diff/v1");
    assert_eq!(report["blocks"].as_array().unwrap().len(), 2);
    assert_eq!(report["blocks"][0]["warmup"], true);
    assert_eq!(report["aggregate"]["missing"], 0);
    let markdown = tokio::fs::read_to_string(out.join("diff.md")).await?;
    println!("CLI stdout:\n{}\ndiff.md:\n{markdown}", String::from_utf8_lossy(&output.stdout));
    assert_eq!(shadow.driver().await?.latest().await?.header.hash, last.header.hash);
    server.stop()?;
    Ok(())
}
