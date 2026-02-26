//! An abstraction for the driver's block executor.
//!
//! This module provides the [`Executor`] trait which abstracts block execution for the driver.
//! The executor is responsible for building and executing blocks from payload attributes,
//! maintaining safe head state, and computing output roots for the execution results.

use alloc::boxed::Box;
use core::error::Error;

use alloy_consensus::{Header, Sealed};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_alloy_rpc_types_engine::OpPayloadAttributes;
use base_proof_executor::BlockBuildingOutcome;

/// Executor trait for block execution in the driver pipeline.
///
/// This trait abstracts the block execution functionality needed by the driver.
/// Implementations are responsible for:
/// - Building blocks from payload attributes
/// - Maintaining execution state and safe head tracking
/// - Computing output roots after block execution
/// - Handling execution errors and recovery scenarios
#[async_trait]
pub trait Executor {
    /// The error type for the Executor.
    type Error: Error;

    /// Waits for the executor to be ready for block execution.
    async fn wait_until_ready(&mut self);

    /// Updates the safe head to the specified header.
    fn update_safe_head(&mut self, header: Sealed<Header>);

    /// Execute the given payload attributes to build and execute a block.
    async fn execute_payload(
        &mut self,
        attributes: OpPayloadAttributes,
    ) -> Result<BlockBuildingOutcome, Self::Error>;

    /// Computes the output root for the most recently executed block.
    fn compute_output_root(&mut self) -> Result<B256, Self::Error>;
}
