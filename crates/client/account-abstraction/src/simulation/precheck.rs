//! UserOperation Prechecks
//!
//! Fast validation checks that run BEFORE simulation to fail fast on obvious issues.
//! These checks are cheaper than full simulation and help prevent DoS attacks.
//!
//! Based on Rundler's precheck implementation:
//! https://github.com/alchemyplatform/rundler/blob/main/crates/sim/src/precheck.rs

use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes, U256};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::estimation::SimulationProvider;
use crate::rpc::UserOperation;

use super::validator::{is_eip7702_delegation, get_eip7702_delegate};

/// Precheck configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecheckConfig {
    /// Maximum total verification gas allowed (account + paymaster verification)
    /// Default: 5_000_000 (5M gas)
    pub max_verification_gas: u64,

    /// Maximum call gas limit allowed
    /// Default: 20_000_000 (20M gas)
    pub max_call_gas_limit: u64,

    /// Maximum total gas cost for a single UserOperation (in wei)
    /// Default: 10 ETH worth of gas
    pub max_total_gas_cost: U256,

    /// Whether to check payer (sender/paymaster) deposit balance
    /// Default: true
    pub check_payer_balance: bool,

    /// Whether to allow EIP-7702 delegated senders
    /// Default: true
    pub allow_eip7702: bool,
}

impl Default for PrecheckConfig {
    fn default() -> Self {
        Self {
            max_verification_gas: 5_000_000,
            max_call_gas_limit: 20_000_000,
            // 10 ETH max gas cost
            max_total_gas_cost: U256::from(10_000_000_000_000_000_000u128),
            check_payer_balance: true,
            allow_eip7702: true,
        }
    }
}

/// Result of precheck validation
#[derive(Debug, Clone)]
pub struct PrecheckResult {
    /// Whether the sender is an EIP-7702 delegated account
    pub sender_is_7702: bool,

    /// The delegate address if sender is 7702
    pub delegate_address: Option<Address>,
}

/// Precheck violation types
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PrecheckViolation {
    /// Sender has no code and no factory/initCode to deploy it
    #[serde(rename_all = "camelCase")]
    SenderNotDeployed {
        sender: Address,
    },

    /// Factory address has no code (not a contract)
    #[serde(rename_all = "camelCase")]
    FactoryNotDeployed {
        factory: Address,
    },

    /// Paymaster address has no code (not a contract)
    #[serde(rename_all = "camelCase")]
    PaymasterNotDeployed {
        paymaster: Address,
    },

    /// Factory specified with EIP-7702 sender (invalid combination)
    #[serde(rename_all = "camelCase")]
    FactoryWithEip7702 {
        factory: Address,
        sender: Address,
    },

    /// EIP-7702 is disabled but sender has delegation
    #[serde(rename_all = "camelCase")]
    Eip7702Disabled {
        sender: Address,
    },

    /// Payer (sender or paymaster) has insufficient deposit
    #[serde(rename_all = "camelCase")]
    InsufficientDeposit {
        payer: Address,
        required: U256,
        available: U256,
    },

    /// Verification gas limit too high
    #[serde(rename_all = "camelCase")]
    VerificationGasTooHigh {
        provided: u64,
        max_allowed: u64,
    },

    /// Call gas limit too high
    #[serde(rename_all = "camelCase")]
    CallGasTooHigh {
        provided: u64,
        max_allowed: u64,
    },

    /// Total gas cost exceeds maximum
    #[serde(rename_all = "camelCase")]
    TotalGasCostTooHigh {
        cost: U256,
        max_allowed: U256,
    },
}

impl std::fmt::Display for PrecheckViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SenderNotDeployed { sender } => {
                write!(f, "Sender {} has no code and no factory to deploy it", sender)
            }
            Self::FactoryNotDeployed { factory } => {
                write!(f, "Factory {} is not a deployed contract", factory)
            }
            Self::PaymasterNotDeployed { paymaster } => {
                write!(f, "Paymaster {} is not a deployed contract", paymaster)
            }
            Self::FactoryWithEip7702 { factory, sender } => {
                write!(
                    f,
                    "Factory {} specified but sender {} has EIP-7702 delegation",
                    factory, sender
                )
            }
            Self::Eip7702Disabled { sender } => {
                write!(f, "EIP-7702 is disabled but sender {} has delegation", sender)
            }
            Self::InsufficientDeposit { payer, required, available } => {
                write!(
                    f,
                    "Payer {} has insufficient deposit: {} required, {} available",
                    payer, required, available
                )
            }
            Self::VerificationGasTooHigh { provided, max_allowed } => {
                write!(
                    f,
                    "Verification gas {} exceeds maximum {}",
                    provided, max_allowed
                )
            }
            Self::CallGasTooHigh { provided, max_allowed } => {
                write!(f, "Call gas limit {} exceeds maximum {}", provided, max_allowed)
            }
            Self::TotalGasCostTooHigh { cost, max_allowed } => {
                write!(f, "Total gas cost {} exceeds maximum {}", cost, max_allowed)
            }
        }
    }
}

impl std::error::Error for PrecheckViolation {}

/// Prechecker for UserOperations
///
/// Runs fast validation checks before simulation to fail fast on obvious issues.
pub struct Prechecker<P> {
    provider: Arc<P>,
    config: PrecheckConfig,
}

impl<P> Prechecker<P>
where
    P: SimulationProvider,
{
    /// Create a new prechecker
    pub fn new(provider: Arc<P>, config: PrecheckConfig) -> Self {
        Self { provider, config }
    }

    /// Run all prechecks on a UserOperation
    ///
    /// Returns Ok(PrecheckResult) if all checks pass, or Err with the first violation found.
    pub async fn check(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
    ) -> Result<PrecheckResult, PrecheckViolation> {
        let sender = match user_op {
            UserOperation::V06(op) => op.sender,
            UserOperation::V07(op) => op.sender,
        };
        let factory = Self::extract_factory(user_op);
        let paymaster = Self::extract_paymaster(user_op);

        debug!(
            target: "aa-precheck",
            sender = %sender,
            factory = ?factory,
            paymaster = ?paymaster,
            "Running prechecks"
        );

        // Get sender bytecode to check for 7702 delegation
        let sender_code = self
            .provider
            .eth_get_code(sender, BlockId::pending())
            .await
            .unwrap_or_else(|e| {
                warn!(
                    target: "aa-precheck",
                    sender = %sender,
                    error = %e,
                    "Failed to get sender code, assuming empty"
                );
                Bytes::default()
            });

        let sender_is_7702 = is_eip7702_delegation(&sender_code);
        let delegate_address = get_eip7702_delegate(&sender_code);

        // P0: Check EIP-7702 constraints
        if sender_is_7702 {
            // Check if 7702 is allowed
            if !self.config.allow_eip7702 {
                return Err(PrecheckViolation::Eip7702Disabled { sender });
            }

            // Factory must be empty for 7702 senders
            if let Some(factory_addr) = factory {
                return Err(PrecheckViolation::FactoryWithEip7702 {
                    factory: factory_addr,
                    sender,
                });
            }
        }

        // P0: Check sender existence
        // If no factory and not 7702, sender must have code
        if factory.is_none() && !sender_is_7702 && is_empty_bytecode(&sender_code) {
            return Err(PrecheckViolation::SenderNotDeployed { sender });
        }

        // P0: Check factory existence (if used)
        if let Some(factory_addr) = factory {
            let factory_code = self
                .provider
                .eth_get_code(factory_addr, BlockId::pending())
                .await
                .unwrap_or_else(|e| {
                    warn!(
                        target: "aa-precheck",
                        factory = %factory_addr,
                        error = %e,
                        "Failed to get factory code, assuming empty"
                    );
                    Bytes::default()
                });

            if is_empty_bytecode(&factory_code) {
                return Err(PrecheckViolation::FactoryNotDeployed { factory: factory_addr });
            }
        }

        // P0: Check paymaster existence (if used)
        if let Some(paymaster_addr) = paymaster {
            let paymaster_code = self
                .provider
                .eth_get_code(paymaster_addr, BlockId::pending())
                .await
                .unwrap_or_else(|e| {
                    warn!(
                        target: "aa-precheck",
                        paymaster = %paymaster_addr,
                        error = %e,
                        "Failed to get paymaster code, assuming empty"
                    );
                    Bytes::default()
                });

            if is_empty_bytecode(&paymaster_code) {
                return Err(PrecheckViolation::PaymasterNotDeployed {
                    paymaster: paymaster_addr,
                });
            }
        }

        // P1: Check gas limits
        self.check_gas_limits(user_op)?;

        // P1: Check payer deposit (if enabled)
        if self.config.check_payer_balance {
            self.check_payer_deposit(user_op, entry_point).await?;
        }

        debug!(
            target: "aa-precheck",
            sender = %sender,
            sender_is_7702 = sender_is_7702,
            delegate = ?delegate_address,
            "Prechecks passed"
        );

        Ok(PrecheckResult {
            sender_is_7702,
            delegate_address,
        })
    }

    /// Check gas limit constraints (P1)
    fn check_gas_limits(&self, user_op: &UserOperation) -> Result<(), PrecheckViolation> {
        // Calculate total verification gas
        let total_verification_gas = match user_op {
            UserOperation::V06(op) => op.verification_gas_limit.to::<u64>(),
            UserOperation::V07(op) => {
                op.verification_gas_limit.to::<u64>()
                    + op.paymaster_verification_gas_limit.to::<u64>()
            }
        };

        if total_verification_gas > self.config.max_verification_gas {
            return Err(PrecheckViolation::VerificationGasTooHigh {
                provided: total_verification_gas,
                max_allowed: self.config.max_verification_gas,
            });
        }

        // Check call gas limit
        let call_gas_limit = match user_op {
            UserOperation::V06(op) => op.call_gas_limit.to::<u64>(),
            UserOperation::V07(op) => op.call_gas_limit.to::<u64>(),
        };

        if call_gas_limit > self.config.max_call_gas_limit {
            return Err(PrecheckViolation::CallGasTooHigh {
                provided: call_gas_limit,
                max_allowed: self.config.max_call_gas_limit,
            });
        }

        // Check total gas cost
        let max_gas_cost = self.calculate_max_gas_cost(user_op);
        if max_gas_cost > self.config.max_total_gas_cost {
            return Err(PrecheckViolation::TotalGasCostTooHigh {
                cost: max_gas_cost,
                max_allowed: self.config.max_total_gas_cost,
            });
        }

        Ok(())
    }

    /// Calculate maximum gas cost for a UserOperation
    fn calculate_max_gas_cost(&self, user_op: &UserOperation) -> U256 {
        match user_op {
            UserOperation::V06(op) => {
                let total_gas = op.pre_verification_gas
                    + op.verification_gas_limit
                    + op.call_gas_limit;
                total_gas * op.max_fee_per_gas
            }
            UserOperation::V07(op) => {
                let total_gas = op.pre_verification_gas
                    + op.verification_gas_limit
                    + op.call_gas_limit
                    + op.paymaster_verification_gas_limit
                    + op.paymaster_post_op_gas_limit;
                total_gas * op.max_fee_per_gas
            }
        }
    }

    /// Extract factory address from UserOperation
    fn extract_factory(user_op: &UserOperation) -> Option<Address> {
        match user_op {
            UserOperation::V06(op) => {
                if op.init_code.len() >= 20 {
                    Some(Address::from_slice(&op.init_code[..20]))
                } else {
                    None
                }
            }
            UserOperation::V07(op) => {
                if op.factory.is_zero() {
                    None
                } else {
                    Some(op.factory)
                }
            }
        }
    }

    /// Extract paymaster address from UserOperation
    fn extract_paymaster(user_op: &UserOperation) -> Option<Address> {
        match user_op {
            UserOperation::V06(op) => {
                if op.paymaster_and_data.len() >= 20 {
                    Some(Address::from_slice(&op.paymaster_and_data[..20]))
                } else {
                    None
                }
            }
            UserOperation::V07(op) => {
                if op.paymaster.is_zero() {
                    None
                } else {
                    Some(op.paymaster)
                }
            }
        }
    }

    /// Extract sender address from UserOperation
    fn extract_sender(user_op: &UserOperation) -> Address {
        match user_op {
            UserOperation::V06(op) => op.sender,
            UserOperation::V07(op) => op.sender,
        }
    }

    /// Check payer deposit balance (P1)
    async fn check_payer_deposit(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
    ) -> Result<(), PrecheckViolation> {
        // Determine who pays - paymaster if present, otherwise sender
        let payer = Self::extract_paymaster(user_op).unwrap_or_else(|| Self::extract_sender(user_op));

        // Get deposit balance from EntryPoint
        let deposit = self.get_deposit(payer, entry_point).await;

        // Calculate required deposit (max gas cost)
        let required = self.calculate_max_gas_cost(user_op);

        if deposit < required {
            warn!(
                target: "aa-precheck",
                payer = %payer,
                required = %required,
                available = %deposit,
                "Insufficient payer deposit"
            );
            return Err(PrecheckViolation::InsufficientDeposit {
                payer,
                required,
                available: deposit,
            });
        }

        Ok(())
    }

    /// Get deposit balance for an address from EntryPoint
    async fn get_deposit(&self, address: Address, entry_point: Address) -> U256 {
        use alloy_sol_types::{sol, SolCall};

        // EntryPoint.balanceOf(address) selector
        sol! {
            function balanceOf(address account) external view returns (uint256);
        }

        let calldata = balanceOfCall { account: address }.abi_encode();

        match self
            .provider
            .eth_call(
                entry_point,
                calldata.into(),
                BlockId::pending(),
                None,
            )
            .await
        {
            Ok(crate::estimation::SimulationCallResult::Success(data)) => {
                if data.len() >= 32 {
                    U256::from_be_slice(&data[..32])
                } else {
                    U256::ZERO
                }
            }
            _ => U256::ZERO,
        }
    }
}

/// Check if bytecode is empty (no code deployed)
fn is_empty_bytecode(code: &Bytes) -> bool {
    code.is_empty() || code.as_ref() == [0u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_empty_bytecode() {
        assert!(is_empty_bytecode(&Bytes::new()));
        assert!(is_empty_bytecode(&Bytes::from(vec![0u8])));
        assert!(!is_empty_bytecode(&Bytes::from(vec![0xef, 0x01, 0x00])));
        assert!(!is_empty_bytecode(&Bytes::from(vec![0x60, 0x80, 0x60, 0x40])));
    }

    #[test]
    fn test_precheck_config_default() {
        let config = PrecheckConfig::default();
        assert_eq!(config.max_verification_gas, 5_000_000);
        assert_eq!(config.max_call_gas_limit, 20_000_000);
        assert!(config.check_payer_balance);
        assert!(config.allow_eip7702);
    }

    #[test]
    fn test_precheck_violation_display() {
        let violation = PrecheckViolation::SenderNotDeployed {
            sender: Address::ZERO,
        };
        assert!(violation.to_string().contains("no code"));

        let violation = PrecheckViolation::VerificationGasTooHigh {
            provided: 10_000_000,
            max_allowed: 5_000_000,
        };
        assert!(violation.to_string().contains("exceeds maximum"));
    }
}
