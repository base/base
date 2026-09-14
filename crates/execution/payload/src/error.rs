//! Error type

/// Base-specific payload building errors.
#[derive(Debug, thiserror::Error)]
pub enum BasePayloadBuilderError {
    /// Thrown when a transaction fails to convert to a
    /// [`alloy_consensus::transaction::Recovered`].
    #[error("failed to convert deposit transaction to RecoveredTx")]
    TransactionEcRecoverFailed,
    /// Thrown when the L1 block info could not be parsed from the calldata of the
    /// first transaction supplied in the payload attributes.
    #[error("failed to parse L1 block info from L1 info tx calldata")]
    L1BlockInfoParseFailed,
    /// Thrown when a database account could not be loaded.
    #[error("failed to load account {0}")]
    AccountLoadFailed(alloy_primitives::Address),
    /// Thrown when force deploy of create2deployer code fails.
    #[error("failed to force create2deployer account code")]
    ForceCreate2DeployerFail,
    /// Thrown when a blob transaction is included in a sequencer's block.
    #[error("blob transaction included in sequencer block")]
    BlobTransactionRejected,
    /// Thrown when `BlockBuilder` refuses to commit a sequencer transaction.
    ///
    /// Resource metering always returns [`alloy_evm::block::CommitChanges::Yes`]
    /// for sequencer transactions. This error is the `Ok(None)` contract of
    /// `execute_transaction_with_commit_condition` under `no_tx_pool`, where
    /// skipping a sequencer transaction would diverge the EL from the proof
    /// executor.
    #[error("sequencer transaction commit was refused")]
    SequencerTransactionCommitRefused,
}
