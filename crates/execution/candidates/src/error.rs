//! Stable admission errors for candidate groups (design §2.5).

/// Why a candidate group was rejected at ingress.
///
/// Every variant maps to a stable, low-cardinality label returned as the JSON-RPC error `data`
/// and used as a metric label.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CandidateError {
    /// The method is registered, but candidate admission is runtime-disabled.
    #[error("candidate admission is disabled")]
    Disabled,
    /// Fewer than two siblings.
    #[error("a candidate group needs at least two siblings")]
    TooFewSiblings,
    /// More siblings than the configured maximum.
    #[error("too many siblings: maximum {max}")]
    TooManySiblings {
        /// Configured maximum.
        max: usize,
    },
    /// A sibling or the whole request exceeds its encoded-size limit.
    #[error("candidate request is too large")]
    RequestTooLarge,
    /// A sibling is not a decodable, recoverable EIP-2718 transaction.
    #[error("failed to decode sibling {index}")]
    SiblingDecodeFailed {
        /// Sibling index in the request.
        index: usize,
    },
    /// A sibling is not a plain EIP-1559 transaction, or its sender is EIP-7702-delegated.
    #[error("sibling {index} has an unsupported transaction type")]
    SiblingTypeUnsupported {
        /// Sibling index in the request.
        index: usize,
    },
    /// Siblings do not share one sender.
    #[error("siblings must share one sender")]
    SiblingSenderMismatch,
    /// Siblings do not share one nonce or chain ID.
    #[error("siblings must share one nonce and chain id")]
    SiblingNonceMismatch,
    /// Siblings do not share identical fee fields.
    #[error("siblings must share identical fee fields")]
    SiblingFeeMismatch,
    /// Siblings do not share one gas limit.
    #[error("siblings must share one gas limit")]
    SiblingGasLimitMismatch,
    /// The shared gas limit exceeds the configured per-sibling maximum.
    #[error("sibling gas limit exceeds the maximum of {max}")]
    SiblingGasLimitTooHigh {
        /// Configured maximum.
        max: u64,
    },
    /// Two siblings have the same hash.
    #[error("duplicate sibling")]
    DuplicateSibling,
    /// `envelope.siblingHashes` does not equal the submitted siblings, in order.
    #[error("envelope sibling hashes do not match the submitted siblings")]
    EnvelopeHashMismatch,
    /// An envelope field is out of range or inconsistent with the siblings.
    #[error("invalid envelope: {reason}")]
    EnvelopeInvalid {
        /// Which field is invalid.
        reason: &'static str,
    },
    /// The envelope requests a score-read context this node does not implement.
    #[error("unsupported score version")]
    ScoreVersionUnsupported,
    /// `expiryBlock` is in the past or beyond the configured window.
    #[error("expiry block is outside the accepted window")]
    ExpiryOutOfWindow,
    /// The envelope signature is malformed or non-canonical.
    #[error("invalid envelope signature")]
    EnvelopeSignatureInvalid,
    /// The envelope was not signed by the siblings' sender.
    #[error("envelope signer is not the sibling sender")]
    EnvelopeSignerNotSender,
    /// The ledger records this group as landed, exhausted, replaced or revoked.
    #[error("candidate group is terminal")]
    GroupTerminal,
    /// The representative hash is already pooled under a different envelope.
    #[error("representative is already pooled under a different envelope")]
    RepresentativeConflict,
    /// The sender maps to no authorized principal.
    #[error("sender is not an authorized candidate principal")]
    PrincipalNotAllowed,
    /// The policy price for the declared budget exceeds `maxAccessCharge`.
    #[error("max access charge is below the policy price")]
    AccessChargeTooLow,
    /// The principal has no work quota left.
    #[error("principal work quota exhausted")]
    PrincipalQuotaExhausted,
    /// The sender already has the maximum number of pooled groups.
    #[error("sender candidate group limit reached")]
    SenderGroupLimit,
    /// The pool already holds the maximum number of groups.
    #[error("pool candidate group limit reached")]
    PoolGroupLimit,
    /// The group ledger is unavailable; admission fails closed.
    #[error("candidate ledger unavailable")]
    LedgerUnavailable,
    /// v0 rejects groups combined with validity predicates.
    #[error("candidate groups cannot carry validity predicates")]
    CombinedWithValidity,
    /// v0 rejects EIP-7702-delegated senders (design §4.2): delegated code can move the sender's
    /// balance or nonce independently of the siblings.
    #[error("EIP-7702-delegated senders cannot submit candidate groups")]
    SenderDelegated,
}

impl CandidateError {
    /// Returns the stable label from design §2.5.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::Disabled => "candidates_disabled",
            Self::TooFewSiblings => "too_few_siblings",
            Self::TooManySiblings { .. } => "too_many_siblings",
            Self::RequestTooLarge => "request_too_large",
            Self::SiblingDecodeFailed { .. } => "sibling_decode_failed",
            Self::SiblingTypeUnsupported { .. } => "sibling_type_unsupported",
            Self::SiblingSenderMismatch => "sibling_sender_mismatch",
            Self::SiblingNonceMismatch => "sibling_nonce_mismatch",
            Self::SiblingFeeMismatch => "sibling_fee_mismatch",
            Self::SiblingGasLimitMismatch => "sibling_gas_limit_mismatch",
            Self::SiblingGasLimitTooHigh { .. } => "sibling_gas_limit_too_high",
            Self::DuplicateSibling => "duplicate_sibling",
            Self::EnvelopeHashMismatch => "envelope_hash_mismatch",
            Self::EnvelopeInvalid { .. } => "envelope_invalid",
            Self::ScoreVersionUnsupported => "score_version_unsupported",
            Self::ExpiryOutOfWindow => "expiry_out_of_window",
            Self::EnvelopeSignatureInvalid => "envelope_signature_invalid",
            Self::EnvelopeSignerNotSender => "envelope_signer_not_sender",
            Self::GroupTerminal => "group_terminal",
            Self::RepresentativeConflict => "representative_conflict",
            Self::PrincipalNotAllowed => "principal_not_allowed",
            Self::AccessChargeTooLow => "access_charge_too_low",
            Self::PrincipalQuotaExhausted => "principal_quota_exhausted",
            Self::SenderGroupLimit => "sender_group_limit",
            Self::PoolGroupLimit => "pool_group_limit",
            Self::LedgerUnavailable => "ledger_unavailable",
            Self::CombinedWithValidity => "combined_with_validity",
            Self::SenderDelegated => "sender_delegated",
        }
    }
}
