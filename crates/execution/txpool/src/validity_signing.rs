//! Wallet-facing EIP-712 types for validity predicate authorization.

use alloy_sol_types::sol;

sol! {
    /// EIP-712 encoding of one predicate; unused fields are zero.
    #[derive(Debug)]
    struct ValidityPredicate {
        /// Stable identifier from `ValidityPredicateKind`.
        uint8 kind;
        /// Stable identifier from `ValidityOperator`.
        uint8 operator;
        /// Account read by a balance, nonce, or storage predicate.
        address account;
        /// Storage slot; zero for other predicates.
        uint256 slot;
        /// Storage mask; zero for other predicates.
        uint256 mask;
        /// Right-hand comparison value.
        uint256 value;
    }

    /// EIP-712 message binding predicates to one signed transaction.
    #[derive(Debug)]
    struct ValidityAuthorization {
        /// Hash of the complete signed EIP-2718 envelope.
        bytes32 transactionHash;
        /// Predicates sorted by their EIP-712 struct hashes, retaining duplicates.
        ValidityPredicate[] validity;
    }
}
