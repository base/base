//! Node service runtime settings.

use crate::ShadowDbConfig;

use crate::ShadowRetentionConfig;
/// Configuration for the shadow indexer extension.
#[derive(Clone, Debug)]
pub struct ShadowIndexerConfig {
    /// Whether the shadow indexer pipeline is enabled.
    pub enabled: bool,
    /// Database configuration for the shadow indexer writer.
    pub db: ShadowDbConfig,
    /// Builder version string to attach to persisted rows.
    pub builder_version: String,
    /// Retention policy that bounds shadow block table growth.
    pub retention: ShadowRetentionConfig,
}
