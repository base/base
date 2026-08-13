//! Composition root for the one-shot (Denim) sequencer payload build path.

use std::time::Duration;

use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_evm::ConfigureEvm;
use reth_tasks::Runtime;

use crate::{builder::BasePayloadBuilder, config::BaseBuilderConfig};

/// Default payload job lifetime.
///
/// Under one-shot freeze semantics a job builds once and never rebuilds, so
/// the deadline is abandoned-job garbage collection only, not a build budget.
/// Its lower bound is set by how late `engine_getPayload` can arrive during
/// HA failover.
pub const DEFAULT_JOB_DEADLINE: Duration = Duration::from_secs(1);

/// All configuration for the one-shot sequencer payload build path, reachable
/// from one struct.
#[derive(Debug, Clone)]
pub struct BasePayloadServiceConfig {
    /// Limits and policy for the payload builder itself.
    pub builder: BaseBuilderConfig,
    /// Payload job lifetime; abandoned-job GC only. Defaults to
    /// [`DEFAULT_JOB_DEADLINE`].
    pub job_deadline: Duration,
}

impl Default for BasePayloadServiceConfig {
    fn default() -> Self {
        Self { builder: BaseBuilderConfig::default(), job_deadline: DEFAULT_JOB_DEADLINE }
    }
}

/// Composition root for the one-shot sequencer payload build path.
///
/// Owns [`BasePayloadServiceConfig`], assembles the [`BasePayloadBuilder`],
/// and instantiates reth's [`BasicPayloadJobGenerator`] around it. It
/// deliberately does not reimplement any job lifecycle: the generator, job,
/// deadline, and resolve semantics stay upstream-owned. The rebuild interval
/// is irrelevant because Denim-active builds freeze after one iteration.
#[derive(Debug, Clone)]
pub struct BasePayloadServiceBuilder {
    /// Configuration for the sequencer build path.
    pub config: BasePayloadServiceConfig,
}

impl BasePayloadServiceBuilder {
    /// Creates a new composition root with the given configuration.
    pub const fn new(config: BasePayloadServiceConfig) -> Self {
        Self { config }
    }

    /// The configured one-shot payload builder.
    pub fn payload_builder<Pool, Client, Evm>(
        &self,
        pool: Pool,
        client: Client,
        evm_config: Evm,
    ) -> BasePayloadBuilder<Pool, Client, Evm>
    where
        Evm: ConfigureEvm,
    {
        BasePayloadBuilder::with_builder_config(
            pool,
            client,
            evm_config,
            self.config.builder.clone(),
        )
    }

    /// Reth's basic payload job generator wrapping the one-shot builder.
    pub fn generator<Pool, Client, Evm>(
        &self,
        pool: Pool,
        client: Client,
        evm_config: Evm,
        executor: Runtime,
    ) -> BasicPayloadJobGenerator<Client, BasePayloadBuilder<Pool, Client, Evm>>
    where
        Client: Clone,
        Evm: ConfigureEvm,
    {
        let job_config =
            BasicPayloadJobGeneratorConfig::default().deadline(self.config.job_deadline);
        BasicPayloadJobGenerator::with_builder(
            client.clone(),
            executor,
            job_config,
            self.payload_builder(pool, client, evm_config),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_gc_deadline() {
        let config = BasePayloadServiceConfig::default();
        assert_eq!(config.job_deadline, DEFAULT_JOB_DEADLINE);
    }
}
