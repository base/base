//! Error type for ERC-8168 payer-service price computation.

/// Errors produced while decoding a price source or computing the phase-0
/// token payment amount for an ERC-8168 offer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    /// The price source resolved to a zero denominator, which would divide by
    /// zero when computing the payment amount.
    #[error("price source has a zero denominator")]
    ZeroDenominator,

    /// A feed-backed price source was resolved without an oracle reading. The
    /// reader layer must supply a [`FeedReading`](crate::FeedReading) for feed
    /// sources.
    #[error("feed-backed price source resolved without an oracle reading")]
    MissingReading,

    /// The oracle answer was zero or negative (top bit of the `int256` set).
    /// Price feeds are expected to be strictly positive.
    #[error("oracle answer is zero or negative")]
    NonPositiveAnswer,

    /// The oracle return data was shorter than the configured answer shape
    /// requires.
    #[error("oracle return data too short: needed {expected} bytes, got {got}")]
    ShortReturnData {
        /// Minimum number of bytes the configured [`AnswerShape`](crate::AnswerShape) needs.
        expected: usize,
        /// Number of bytes actually present in the return data.
        got: usize,
    },

    /// The configured answer shape carries no update timestamp, yet a non-zero
    /// staleness bound was set. Staleness cannot be enforced for such a shape.
    #[error("staleness bound set on a feed shape that carries no update timestamp")]
    StalenessUnsupported,

    /// The oracle answer is older than the configured staleness bound.
    #[error("oracle answer is stale: updated {age}s ago, bound is {bound}s")]
    StaleAnswer {
        /// Age of the answer in seconds (`now - updatedAt`).
        age: u64,
        /// Configured maximum permitted age in seconds.
        bound: u64,
    },

    /// A configured decimals value is too large to raise 10 to without
    /// overflowing a `U256`.
    #[error("decimals value {0} is too large")]
    DecimalsTooLarge(u8),

    /// A `U256` arithmetic operation overflowed while computing the rate or the
    /// payment amount.
    #[error("arithmetic overflow while computing token payment amount")]
    Overflow,
}
