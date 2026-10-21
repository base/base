//! One-shot fee collection jobs.

#[tokio::main]
async fn main() {
    base_cli_utils::run_cli_main!(async base_fee_jobs::Cli);
}
