//! Contains EIP-8130 account-abstraction transaction parts.
pub use base_common_consensus::EIP8130_TX_TYPE_ID as EIP8130_TRANSACTION_TYPE;
use base_common_consensus::Eip8130Signed;

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
    /// When `true`, this is a read-only RPC simulation built from an
    /// `eth_estimateGas` / `eth_call` request (no recoverable signature) and is
    /// routed to `Eip8130Executor::simulate` — which skips signature
    /// verification and fee settlement and reverts all state — instead of
    /// `Eip8130Executor::execute`. Set only on the RPC call path; the
    /// consensus/block-execution path always leaves it `false`, so block
    /// execution and txpool admission never reach the unverified path.
    pub simulate: bool,
}

impl Eip8130TransactionParts {
    /// Create new EIP-8130 transaction parts from a signed envelope, for the
    /// verified consensus/block-execution path.
    pub const fn new(signed: Eip8130Signed) -> Self {
        Self { signed, simulate: false }
    }

    /// Create EIP-8130 transaction parts for a read-only RPC simulation
    /// (`eth_estimateGas` / `eth_call`), routed to `Eip8130Executor::simulate`.
    pub const fn new_simulation(signed: Eip8130Signed) -> Self {
        Self { signed, simulate: true }
    }
}
