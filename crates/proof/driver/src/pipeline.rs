//! Abstracts the derivation pipeline from the driver.
//!
//! This module provides the [`DriverPipeline`] trait which serves as a high-level
//! abstraction for the driver's derivation pipeline. The pipeline is responsible
//! for deriving L2 blocks from L1 data and producing payload attributes for execution.

use alloc::boxed::Box;

use async_trait::async_trait;
use base_consensus_derive::{
    ActivationSignal, Pipeline, PipelineError, PipelineErrorKind, ResetError, ResetSignal,
    SignalReceiver, StepResult,
};
use base_protocol::{L2BlockInfo, OpAttributesWithParent};

/// High-level abstraction for the driver's derivation pipeline.
///
/// The [`DriverPipeline`] trait extends the base [`Pipeline`] functionality with
/// driver-specific operations needed for block production. It handles the complex
/// logic of stepping through derivation stages, managing resets and reorgs, and
/// producing payload attributes for block building.
#[async_trait]
pub trait DriverPipeline<P>: Pipeline + SignalReceiver
where
    P: Pipeline + SignalReceiver,
{
    /// Flushes any cached data due to a reorganization.
    fn flush(&mut self);

    /// Produces payload attributes for the next block after the given L2 safe head.
    async fn produce_payload(
        &mut self,
        l2_safe_head: L2BlockInfo,
    ) -> Result<OpAttributesWithParent, PipelineErrorKind> {
        // As we start the safe head at the disputed block's parent, we step the pipeline until the
        // first attributes are produced. All batches at and before the safe head will be
        // dropped, so the first payload will always be the disputed one.
        loop {
            match self.step(l2_safe_head).await {
                StepResult::PreparedAttributes => {
                    info!(target: "client_derivation_driver", "Stepped derivation pipeline")
                }
                StepResult::AdvancedOrigin => {
                    info!(
                        target: "client_derivation_driver",
                        l1_block_number = self.origin().map(|o| o.number).ok_or(PipelineError::MissingOrigin.crit())?,
                        "Advanced origin"
                    )
                }
                StepResult::OriginAdvanceErr(e) | StepResult::StepFailed(e) => {
                    // Break the loop unless the error signifies that there is not enough data to
                    // complete the current step. In this case, we retry the step to see if other
                    // stages can make progress.
                    match e {
                        PipelineErrorKind::Temporary(_) => {
                            trace!(target: "client_derivation_driver", error = ?e, "Failed to step derivation pipeline temporarily");
                            continue;
                        }
                        PipelineErrorKind::Reset(e) => {
                            warn!(target: "client_derivation_driver", error = ?e, "Failed to step derivation pipeline due to reset");
                            let system_config = self
                                .system_config_by_number(l2_safe_head.block_info.number)
                                .await?;

                            if matches!(e, ResetError::HoloceneActivation) {
                                let l1_origin =
                                    self.origin().ok_or(PipelineError::MissingOrigin.crit())?;
                                self.signal(
                                    ActivationSignal {
                                        l2_safe_head,
                                        l1_origin,
                                        system_config: Some(system_config),
                                    }
                                    .signal(),
                                )
                                .await?;
                            } else {
                                // Flushes cache if a reorg is detected.
                                if matches!(e, ResetError::ReorgDetected(_, _)) {
                                    self.flush();
                                }

                                // Reset the pipeline to the initial L2 safe head and L1 origin,
                                // and try again.
                                let l1_origin =
                                    self.origin().ok_or(PipelineError::MissingOrigin.crit())?;
                                self.signal(
                                    ResetSignal {
                                        l2_safe_head,
                                        l1_origin,
                                        system_config: Some(system_config),
                                    }
                                    .signal(),
                                )
                                .await?;
                            }
                        }
                        PipelineErrorKind::Critical(_) => {
                            warn!(target: "client_derivation_driver", error = ?e, "Failed to step derivation pipeline");
                            return Err(e);
                        }
                    }
                }
            }

            if let Some(attrs) = self.next() {
                return Ok(attrs);
            }
        }
    }
}
