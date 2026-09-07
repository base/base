//! Full-block payload service configuration.

use base_execution_payload_builder::config::BaseBuilderConfig;
use base_node_core::BasePayloadServiceBuilder;

use crate::BuilderConfig;

/// Converts sequencer settings into the concrete Base payload service.
#[derive(Debug)]
pub struct BlockServiceBuilder;

impl BlockServiceBuilder {
    /// Configures full-block payload construction and its deadline.
    pub fn build(builder_config: BuilderConfig) -> BasePayloadServiceBuilder {
        BasePayloadServiceBuilder::full_block(
            BaseBuilderConfig {
                da_config: builder_config.da_config,
                gas_limit_config: builder_config.gas_limit_config,
                manifest_precheck_enabled: builder_config.manifest_precheck_enabled,
                predicate_eval_hard_cutoff: builder_config.predicate_eval_hard_cutoff,
                max_gas_per_txn: builder_config.max_gas_per_txn,
                max_uncompressed_block_size: builder_config.max_uncompressed_block_size,
            },
            builder_config.block_time.saturating_add(builder_config.block_time_leeway),
        )
    }
}
