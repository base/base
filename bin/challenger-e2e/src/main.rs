//! Standalone challenger E2E binary for K8s `CronJob` execution.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().with_env_filter(EnvFilter::from_default_env()).init();

    tracing::info!("starting challenger E2E test");

    // `verdict` matches the field the per-phase logs use, so one Datadog query
    // covers both the run and its phases: `@data.message.fields.verdict:fail`.
    if let Err(e) = base_challenger_e2e::ChallengerE2e::run().await {
        // `infra` separates "a dependency was down" from "the challenger
        // regressed"; the distinct exit code lets the Workflow retry only the
        // former.
        let infra = base_challenger_e2e::is_infra(&e);
        tracing::error!(
            verdict = "fail",
            infra,
            error = %e,
            error_debug = ?e,
            "challenger E2E test failed"
        );
        std::process::exit(if infra { base_challenger_e2e::INFRA_EXIT_CODE } else { 1 });
    }

    tracing::info!(verdict = "pass", "challenger E2E test passed");
}
