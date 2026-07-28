//! Built-in B-20 policy slot identifiers.

use alloy_primitives::{B256, b256};

const TRANSFER_SENDER_POLICY: B256 =
    b256!("b81736c875ab819dd97f59f2a6542cfb731ad52b4ae15a6f24df2fb02b0327f5");
const TRANSFER_RECEIVER_POLICY: B256 =
    b256!("8a4b3fa2d8b921852bc0089c6ef0958aa6961897be36fd731330fe2cd23f8363");
const TRANSFER_EXECUTOR_POLICY: B256 =
    b256!("10be5173aff2a44e748bd9acd8b19fe34689581398a9db7ba2fb671e786ff7d8");
const MINT_RECEIVER_POLICY: B256 =
    b256!("a0d5ae037e66a09119acf080a1d807abb9b6d03b6b9130eb19f7c1e6bdb8ffc8");
const SEIZABLE_ACCOUNT_POLICY: B256 =
    b256!("3efcaab33335f8757bc9054f42ac0ae92950f9727a52ff4db8681fc9521c9e25");

/// Built-in B-20 policy slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20PolicyType {
    /// Policy slot checked against transfer senders.
    TransferSender,
    /// Policy slot checked against transfer receivers.
    TransferReceiver,
    /// Policy slot checked against delegated transfer executors.
    TransferExecutor,
    /// Policy slot checked against mint receivers.
    MintReceiver,
    /// Policy slot consulted against `from` by the seize operations. A `from` is seizable only when
    /// it is NOT authorized by this policy (mirroring the `burnBlocked` "blocked" semantics).
    SeizableAccount,
}

impl B20PolicyType {
    /// Returns the built-in policy type for `id`, if it is recognized.
    pub fn from_id(id: B256) -> Option<Self> {
        if id == TRANSFER_SENDER_POLICY {
            Some(Self::TransferSender)
        } else if id == TRANSFER_RECEIVER_POLICY {
            Some(Self::TransferReceiver)
        } else if id == TRANSFER_EXECUTOR_POLICY {
            Some(Self::TransferExecutor)
        } else if id == MINT_RECEIVER_POLICY {
            Some(Self::MintReceiver)
        } else if id == SEIZABLE_ACCOUNT_POLICY {
            Some(Self::SeizableAccount)
        } else {
            None
        }
    }

    /// Returns the policy type identifier.
    pub const fn id(self) -> B256 {
        match self {
            Self::TransferSender => TRANSFER_SENDER_POLICY,
            Self::TransferReceiver => TRANSFER_RECEIVER_POLICY,
            Self::TransferExecutor => TRANSFER_EXECUTOR_POLICY,
            Self::MintReceiver => MINT_RECEIVER_POLICY,
            Self::SeizableAccount => SEIZABLE_ACCOUNT_POLICY,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use crate::B20PolicyType;

    /// Cross-impl parity: the built-in scope ids must equal `keccak256` of the exact strings
    /// base-std uses, so the two implementations cannot silently diverge.
    #[test]
    fn policy_ids_match_base_std_keccak() {
        assert_eq!(B20PolicyType::TransferSender.id(), keccak256("TRANSFER_SENDER_POLICY"));
        assert_eq!(B20PolicyType::TransferReceiver.id(), keccak256("TRANSFER_RECEIVER_POLICY"));
        assert_eq!(B20PolicyType::TransferExecutor.id(), keccak256("TRANSFER_EXECUTOR_POLICY"));
        assert_eq!(B20PolicyType::MintReceiver.id(), keccak256("MINT_RECEIVER_POLICY"));
        assert_eq!(B20PolicyType::SeizableAccount.id(), keccak256("SEIZABLE_ACCOUNT_POLICY"));
    }

    /// `from_id` is the inverse of `id`, including for the seizable scope.
    #[test]
    fn seizable_scope_round_trips() {
        assert_eq!(
            B20PolicyType::from_id(B20PolicyType::SeizableAccount.id()),
            Some(B20PolicyType::SeizableAccount)
        );
    }
}
