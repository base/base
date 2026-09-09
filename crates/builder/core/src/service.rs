//! Full-block payload service configuration.

use base_execution_payload_builder::config::BaseBuilderConfig;
use base_node_core::BasePayloadServiceConfig;

use crate::BuilderConfig;

impl BuilderConfig {
    /// Configures full-block payload construction and its deadline.
    pub fn into_payload_service_config(self) -> BasePayloadServiceConfig {
        BasePayloadServiceConfig::full_block(
            BaseBuilderConfig {
                da_config: self.da_config,
                gas_limit_config: self.gas_limit_config,
                manifest_precheck_enabled: self.manifest_precheck_enabled,
                predicate_eval_hard_cutoff: self.predicate_eval_hard_cutoff,
                max_gas_per_txn: self.max_gas_per_txn,
                max_uncompressed_block_size: self.max_uncompressed_block_size,
                ..Default::default()
            },
            self.block_time.saturating_add(self.block_time_leeway),
        )
    }
}
