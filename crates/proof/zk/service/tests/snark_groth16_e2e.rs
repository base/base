//! End-to-end integration test for the SNARK Groth16 two-stage proving pipeline.
//!
//! Delegates to [`SnarkE2e::run()`], which is also used by the standalone
//! `base-snark-e2e` binary (K8s `CronJob`).
//!
//! Requires:
//! - A running prover-service with real node endpoints
//! - `L2_NODE_ADDRESS` environment variable
//!
//! Auto-skips when `L2_NODE_ADDRESS` is not set.
//!
#[tokio::test]
#[ignore = "requires a running prover-service and L2_NODE_ADDRESS; run with `cargo nextest run --run-ignored all -p base-zk-service --test snark_groth16_e2e`"]
async fn snark_groth16_e2e_prove_and_verify() {
    if std::env::var("L2_NODE_ADDRESS").is_err() {
        println!("Skipping: L2_NODE_ADDRESS not set.");
        return;
    }

    // Initialize tracing so the shared module's tracing::info! calls produce output.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    base_zk_service::SnarkE2e::run().await.expect("SNARK e2e test failed");
}
