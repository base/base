//! Contains EIP-8130 account-abstraction transaction parts.
pub use base_common_consensus::EIP8130_TX_TYPE_ID as EIP8130_TRANSACTION_TYPE;
use base_common_consensus::Eip8130Signed;

/// Execution mode for an EIP-8130 transaction.
///
/// This is a type-safe trust boundary: [`Self::Simulate`] routes to
/// `Eip8130Executor::simulate`, which skips signature verification and fee
/// settlement and reverts all state. The consensus/block-execution and txpool
/// paths only ever construct [`Self::Verified`], so an unverified transaction
/// can never reach block inclusion. Using an enum (rather than a bare `bool`)
/// makes it impossible to silently flip the gate on the consensus path without
/// an explicit, reviewable match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Eip8130ExecutionMode {
    /// Full consensus/block-execution path: signature verification and fee
    /// settlement run, state is committed. The only mode produced off the RPC
    /// simulation path.
    #[default]
    Verified,
    /// Read-only RPC simulation (`eth_estimateGas` / `eth_call`): no signature
    /// verification, no fee settlement, all state reverted. Reachable only from
    /// the RPC call path.
    Simulate,
}

impl Eip8130ExecutionMode {
    /// Whether this is the read-only simulation mode.
    pub const fn is_simulate(self) -> bool {
        matches!(self, Self::Simulate)
    }
}

/// EIP-8130 account-abstraction transaction parts carried on a
/// [`BaseTransaction`].
///
/// Unlike the other transaction types, an EIP-8130 transaction cannot be fully
/// expressed as a revm `TxEnv`: it has a sender/payer split, a list of phased
/// calls (`Vec<Vec<Call>>`), and account-configuration changes that are applied
/// before execution. The `TxEnv` projection built by `from_encoded_tx` is only a
/// placeholder; the full signed envelope is carried here so the handler can run
/// the EIP-8130 authorize → apply → execute pipeline, which needs the
/// sender/payer authentication blobs and the account changes the `TxEnv` cannot
/// hold.
///
/// [`BaseTransaction`]: crate::BaseTransaction
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130TransactionParts {
    /// The signed EIP-8130 envelope: the transaction body plus the sender and
    /// (optional) payer authentication blobs.
    pub signed: Eip8130Signed,
    /// Execution mode. [`Eip8130ExecutionMode::Simulate`] is set only on the RPC
    /// call path; the consensus/block-execution path always leaves it
    /// [`Eip8130ExecutionMode::Verified`], so block execution and txpool
    /// admission never reach the unverified path.
    pub mode: Eip8130ExecutionMode,
}

impl Eip8130TransactionParts {
    /// Create new EIP-8130 transaction parts from a signed envelope, for the
    /// verified consensus/block-execution path. The RPC simulation path builds
    /// parts through the consensus-tx conversion and then sets
    /// [`Eip8130ExecutionMode::Simulate`] on the result.
    pub const fn new(signed: Eip8130Signed) -> Self {
        Self { signed, mode: Eip8130ExecutionMode::Verified }
    }
}
