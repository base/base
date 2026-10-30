//! Recorded builder ingress through HTTP to a real builder's insertion handler over IPC.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use alloy_consensus::TxEip1559;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, TxKind, keccak256};
use alloy_provider::Provider;
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
    test_utils::{LocalInstanceBuilder, funded_signer, sign_base_tx},
};
use base_clock::{GlobalClock, OffsetClock};
use base_execution_txpool::{NoExtensions, ValidatedTransaction};
use base_replay::{ReplayRecord, ReplayStream};
use base_replay_driver::{BuilderIngressInjector, InjectionSchedule, Injector};
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn replays_verbatim_calls_when_builder_clock_reaches_recorded_offsets() -> eyre::Result<()> {
    GlobalClock::set(OffsetClock::new(UNIX_EPOCH + Duration::from_secs(1_700_000_000)))?;
    let builder = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let provider = builder.provider().await?;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut rpc = RpcModule::new((provider.clone(), Arc::clone(&observed)));
    for method in ["base_insertValidatedTransaction", "base_insertValidatedTransactions"] {
        rpc.register_async_method(method, move |params, ctx, _| async move {
            let raw: Box<RawValue> = params.parse()?;
            ctx.1
                .lock()
                .unwrap()
                .push((GlobalClock::get().now_unix_millis().unwrap(), raw.get().to_owned()));
            ctx.0
                .raw_request::<_, Box<RawValue>>(method.into(), raw)
                .await
                .map_err(|err| ErrorObjectOwned::owned(-32000, err.to_string(), None::<()>))
        })?;
    }
    let server = Server::builder().build("127.0.0.1:0").await?;
    let url = format!("http://{}", server.local_addr()?).parse()?;
    let server = server.start(rpc);
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()?;
    let injector = BuilderIngressInjector::new(url, &runtime)?;
    let start = u64::try_from(GlobalClock::get().now_unix_millis()?)?;
    let mut records = Vec::new();
    let mut hashes = Vec::new();
    for (nonce, offset) in [1000, 1200, 1400].into_iter().enumerate() {
        let tx = sign_base_tx(
            &funded_signer(),
            TxEip1559 {
                chain_id: 901,
                nonce: u64::try_from(nonce)?,
                gas_limit: 21_000,
                max_fee_per_gas: 1_000_000_000,
                to: TxKind::Call(Address::ZERO),
                ..Default::default()
            }
            .into(),
        )?;
        let transaction = ValidatedTransaction {
            sender: tx.signer(),
            raw: tx.encoded_2718().into(),
            metering: None,
            extensions: NoExtensions {},
        };
        let hash = keccak256(&transaction.raw);
        hashes.push(hash);
        let encoded = serde_json::to_string(&transaction)?;
        let (method, params) = if nonce == 2 {
            ("base_insertValidatedTransactions", format!("[[ {encoded} ]]"))
        } else {
            ("base_insertValidatedTransaction", format!("[ {encoded} ]"))
        };
        records.push(ReplayRecord {
            stream: ReplayStream::BuilderIngress,
            received_at_ms: start + offset,
            method: method.into(),
            params: RawValue::from_string(params)?,
            batch_id: Some(0),
            source: "127.0.0.1:1234".into(),
            tx_hash: Some(hash),
        });
    }
    let mut schedule =
        InjectionSchedule::new(vec![records[2].clone(), records[0].clone(), records[1].clone()]);
    schedule.warmup(&injector, start + 1000).await?;
    assert_eq!(observed.lock().unwrap().len(), 1, "warmup is acknowledged before first FCU");
    schedule.run(&injector, &CancellationToken::new()).await?;
    for (index, (observed_at, params)) in observed.lock().unwrap().iter().enumerate() {
        let recorded = records[index].received_at_ms - start;
        let actual = observed_at - u128::from(start);
        assert_eq!(params, records[index].params.get());
        if index == 0 {
            assert!(actual < u128::from(recorded));
        } else {
            assert!((u128::from(recorded)..u128::from(recorded + 300)).contains(&actual));
        }
        println!(
            "builder ingress: recorded_offset_ms={recorded}, observed_offset_ms={actual}, warmup={}",
            index == 0
        );
    }
    for hash in &hashes {
        assert!(provider.get_transaction_by_hash(*hash).await?.is_some());
    }
    println!("builder pool: {} recorded transactions present", hashes.len());
    let mut rejected = records[0].clone();
    rejected.method = "replay_unknown_method".into();
    let error = injector.inject(rejected).await.unwrap_err();
    assert!(error.downcast_ref::<Arc<jsonrpsee::core::ClientError>>().is_some());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(schedule.run(&injector, &cancel).await.is_err());
    drop(injector);
    server.stop()?;
    server.stopped().await;
    Ok(())
}
