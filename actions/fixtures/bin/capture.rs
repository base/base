//! Localized fixture capture command for `base-action-fixtures`.

use std::path::Path;

use base_action_fixtures::CaptureCommand;
use clap::Parser;

#[tokio::main]
async fn main() {
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    let _ = dotenvy::from_path(env_path);

    if let Err(error) = CaptureCommand::parse().run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
