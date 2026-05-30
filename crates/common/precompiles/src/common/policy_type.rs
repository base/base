//! Built-in B-20 policy slot identifiers.
//!
//! [`B20PolicyType`] names every built-in policy scope across B-20 token variants.
//! Core slots (transfer sender/receiver/executor and mint receiver) apply to all
//! variants; security-only slots such as redeem sender are gated per variant via
//! [`crate::PolicyManaged::supports_policy_scope`].

use alloy_primitives::{B256, b256};

const TRANSFER_SENDER_POLICY: B256 =
    b256!("b81736c875ab819dd97f59f2a6542cfb731ad52b4ae15a6f24df2fb02b0327f5");
const TRANSFER_RECEIVER_POLICY: B256 =
    b256!("8a4b3fa2d8b921852bc0089c6ef0958aa6961897be36fd731330fe2cd23f8363");
const TRANSFER_EXECUTOR_POLICY: B256 =
    b256!("10be5173aff2a44e748bd9acd8b19fe34689581398a9db7ba2fb671e786ff7d8");
const MINT_RECEIVER_POLICY: B256 =
    b256!("a0d5ae037e66a09119acf080a1d807abb9b6d03b6b9130eb19f7c1e6bdb8ffc8");
const REDEEM_SENDER_POLICY: B256 =
    b256!("0ff53b08b65363a609bb561211128f4044adc0e351f0b92b6aa23f8d85462f59");

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
    /// Policy slot checked against redeem senders. Security variant only.
    RedeemSender,
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
        } else if id == REDEEM_SENDER_POLICY {
            Some(Self::RedeemSender)
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
            Self::RedeemSender => REDEEM_SENDER_POLICY,
        }
    }

    /// Returns `true` for policy slots shared by every B-20 variant.
    pub const fn is_core(self) -> bool {
        !matches!(self, Self::RedeemSender)
    }

    /// Returns `true` for policy slots defined only on a specific variant.
    pub const fn is_security(self) -> bool {
        matches!(self, Self::RedeemSender)
    }
}
