//! Logging and metrics initialization.

use crate::{cli::UnifiedArgs, version::VersionInfo};

/// Initializer for logging and metrics.
#[derive(Debug, Clone)]
pub struct Setup;

impl Setup {
    /// Initializes logging and metrics from [`UnifiedArgs`].
    pub fn init(args: &UnifiedArgs) -> eyre::Result<()> {
        base_cli_utils::LogConfig::from(args.global.logging.clone()).init_tracing_subscriber()?;

        args.global.metrics.init_with(|| {
            VersionInfo::from_build().register_version_metrics();
        })?;

        Ok(())
    }
}
