//! Contains Base transaction parts.
pub use base_common_consensus::BASE_TX_TYPE_ID as BASE_TRANSACTION_TYPE;
use base_common_consensus::TxBaseSigned;

/// Execution mode for a Base transaction.
///
/// This is a type-safe trust boundary: [`Self::Simulate`] routes to
/// `TxBaseExecutor::simulate`, which skips signature verification and fee
/// settlement and reverts all state. The consensus/block-execution and txpool
/// paths only ever construct [`Self::Verified`], so an unverified transaction
/// can never reach block inclusion. Using an enum (rather than a bare `bool`)
/// makes it impossible to silently flip the gate on the consensus path without
/// an explicit, reviewable match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TxBaseExecutionMode {
    /// Full consensus/block-execution path: signature verification and fee
    /// settlement run, state is committed. The only mode produced off the RPC
    /// simulation path.
    #[default]
    Verified,
    /// Read-only RPC gas estimation (`eth_estimateGas`): no signature
    /// verification, no fee settlement, all state reverted. The calls are
    /// re-run at candidate gas pools to find the minimum gas limit that
    /// succeeds. Reachable only from the RPC call path.
    Simulate,
    /// Read-only RPC call (`eth_call`): the same unverified, reverted pipeline
    /// as [`Self::Simulate`], but the calls run exactly once at the
    /// transaction's gas limit, with no gas-limit search. Reachable only from
    /// the RPC call path.
    Call,
}

impl TxBaseExecutionMode {
    /// Whether this is a read-only simulation mode ([`Self::Simulate`] or
    /// [`Self::Call`]).
    pub const fn is_simulate(self) -> bool {
        matches!(self, Self::Simulate | Self::Call)
    }
}

/// Base transaction parts carried on a
/// [`BaseTransaction`].
///
/// Unlike the other transaction types, a Base transaction cannot be fully
/// expressed as a revm `TxEnv`: it has a sender/payer split, a list of phased
/// calls (`Vec<Vec<Call>>`), and a delegation entry installed before
/// execution. The `TxEnv` projection built by `from_encoded_tx` is only a
/// placeholder; the full signed envelope is carried here so the handler can run
/// the Base transaction authorize → delegate → execute pipeline, which needs the
/// sender/payer authentication blobs and the account changes the `TxEnv` cannot
/// hold.
///
/// [`BaseTransaction`]: crate::BaseTransaction
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TxBaseTransactionParts {
    /// The signed Base transaction envelope: the transaction body plus the sender and
    /// (optional) payer authentication blobs.
    pub signed: TxBaseSigned,
    /// Execution mode. [`TxBaseExecutionMode::Simulate`] is set only on the RPC
    /// call path; the consensus/block-execution path always leaves it
    /// [`TxBaseExecutionMode::Verified`], so block execution and txpool
    /// admission never reach the unverified path.
    pub mode: TxBaseExecutionMode,
}

impl TxBaseTransactionParts {
    /// Create new Base transaction parts from a signed envelope, for the
    /// verified consensus/block-execution path. The RPC simulation path builds
    /// parts through the consensus-tx conversion and then sets
    /// [`TxBaseExecutionMode::Simulate`] on the result.
    pub const fn new(signed: TxBaseSigned) -> Self {
        Self { signed, mode: TxBaseExecutionMode::Verified }
    }
}
