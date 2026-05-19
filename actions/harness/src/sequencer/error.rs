use base_consensus_node::ConductorError;

/// Error type returned by [`crate::L2Sequencer`].
#[derive(Debug, thiserror::Error)]
pub enum L2SequencerError {
    /// The L1 block required for the current epoch is missing from the chain.
    #[error("L1 block {0} not found in shared chain")]
    MissingL1Block(u64),
    /// Failed to build the L1 info deposit transaction.
    #[error("failed to build L1 info deposit: {0}")]
    L1Info(#[from] base_protocol::BlockInfoError),
    /// Transaction signing failed.
    #[error("signing failed: {0}")]
    Signing(#[from] alloy_signer::Error),
    /// EVM execution failed.
    #[error("EVM execution failed: {0}")]
    Evm(String),
    /// Origin selection failed.
    #[error("origin selection failed: {0}")]
    OriginSelection(String),
    /// Attributes construction failed.
    #[error("attributes construction failed: {0}")]
    Attributes(String),
    /// Engine client error.
    #[error("engine client error: {0}")]
    Engine(String),
    /// Payload conversion error.
    #[error("payload conversion error: {0}")]
    PayloadConversion(String),
    /// Conductor rejected the block (e.g. not leader, RPC error).
    #[error("conductor error: {0}")]
    Conductor(#[from] ConductorError),
    /// This sequencer is not the conductor leader and cannot build blocks.
    #[error("sequencer is not the conductor leader")]
    NotLeader,
    /// The production sequencer actor failed.
    #[error("sequencer actor error: {0}")]
    Actor(String),
    /// The production sequencer actor did not insert a block before the timeout.
    #[error("sequencer actor timed out waiting for inserted block")]
    Timeout,
    /// The inserted-block notification channel closed before a block was produced.
    #[error("sequencer actor exited before inserting a block")]
    InsertChannelClosed,
    /// The sequencer actor admin API failed.
    #[error("sequencer actor admin error: {0}")]
    Admin(String),
}
