//! Abstraction traits for the registration driver.

use async_trait::async_trait;

use crate::{ProverInstance, Result};

/// Discovers active prover instances from the infrastructure layer.
///
/// The primary implementation is [`AwsTargetGroupDiscovery`], which queries
/// an ALB target group via the AWS SDK. Other implementations (e.g., a static
/// list for local testing) can be substituted.
#[async_trait]
pub trait InstanceDiscovery: Send + Sync {
    /// Return the current set of prover instances with their health status.
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>>;
}
