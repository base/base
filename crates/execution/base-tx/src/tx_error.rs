//! Errors returned by transaction authorization.

/// Reason a transaction's sender or payer could not be authorized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxAuthError {
    /// The EOA-path sender signature was malformed or did not recover (wrong
    /// length, or a non-canonical upper-half `s` rejected by the checked
    /// recovery). Applies only when `tx.sender` is `None`.
    #[error("EOA sender signature could not be recovered")]
    SenderRecovery,

    /// The open-payer `payer_auth` was malformed or did not recover. Applies
    /// only in open payer mode.
    #[error("open payer signature could not be recovered")]
    PayerRecovery,

    /// The [`crate::ResolvedSender`] passed to the authorizer was resolved from
    /// a different transaction.
    #[error("sender was resolved from a different transaction")]
    ResolvedSenderMismatch,

    /// A self-paid transaction (`payer == None`) carries a non-empty
    /// `payer_auth`. The sender's signature does not cover `payer_auth`, so
    /// accepting one would let a relayer change the transaction hash and raise
    /// the sender's L1 data fee.
    #[error("self-paid transaction carries a payer_auth")]
    UnexpectedPayerAuth,

    /// A named sender or payer selects an authenticator other than
    /// [`K1_AUTHENTICATOR`](base_common_consensus::TxBaseConstants::K1_AUTHENTICATOR).
    #[error("unsupported authenticator")]
    UnsupportedAuthenticator,

    /// A named sender or payer's native secp256k1 signature was malformed or
    /// did not recover.
    #[error("secp256k1 signature could not be recovered")]
    SignatureRecovery,

    /// A named sender or payer's native secp256k1 signature recovered to a
    /// different address than the one named.
    #[error("secp256k1 signature does not recover to the named account")]
    SignerMismatch,

    /// More than [`base_common_consensus::TxBaseConstants::MAX_ACCOUNT_CHANGES_PER_TX`]
    /// account changes in a single transaction.
    #[error("at most one delegation entry is allowed")]
    MultipleDelegations,
}
