//! Mempool Rules
//!
//! Implements ERC-7562 mempool-specific validation rules:
//! - STO-040: Entity address used as account elsewhere
//! - STO-041: Associated storage conflict with other sender
//! - AUTH-040: Multiple UserOps from same sender must have same delegate

use alloy_primitives::Address;

use crate::simulation::Erc7562Rule;

use super::error::{MempoolError, MempoolResult};
use super::pool::{EntryPointPool, PooledUserOp};

/// Checks mempool-specific rules that depend on other UserOps in the pool
#[derive(Debug, Default)]
pub struct MempoolRuleChecker;

impl MempoolRuleChecker {
    /// Create a new rule checker
    pub fn new() -> Self {
        Self
    }

    /// Check all mempool rules for a UserOp
    ///
    /// This should be called after basic validation but before adding to pool.
    pub fn check(&self, user_op: &PooledUserOp, pool: &EntryPointPool) -> MempoolResult<()> {
        self.check_sto040(user_op, pool)?;
        self.check_sto041(user_op, pool)?;
        self.check_auth040(user_op, pool)?;
        Ok(())
    }

    /// STO-040: Entity address used as account elsewhere
    ///
    /// An entity (paymaster, factory, aggregator) cannot be used as a sender
    /// in another UserOp in the mempool.
    ///
    /// This prevents an entity from invalidating UserOps that depend on it
    /// by submitting its own UserOp that changes its state.
    fn check_sto040(&self, user_op: &PooledUserOp, pool: &EntryPointPool) -> MempoolResult<()> {
        // Check if any of our entities are used as senders elsewhere
        let entities = [user_op.paymaster, user_op.factory, user_op.aggregator];

        for entity in entities.into_iter().flatten() {
            if pool.is_sender_address(&entity) {
                return Err(MempoolError::rule_violation(
                    Erc7562Rule::Sto040EntityUsedAsAccount,
                    format!(
                        "Entity {} is already used as sender in another UserOp",
                        entity
                    ),
                    None,
                ));
            }
        }

        // Also check: if our sender is used as an entity in another UserOp
        // This is checked implicitly when the other UserOp was added
        // But we should also reject if we're using a sender that's an entity elsewhere
        // This would require tracking entities separately, which we don't do yet
        // TODO: Track entities and check reverse direction

        Ok(())
    }

    /// STO-041: Associated storage conflict with other sender
    ///
    /// A UserOp cannot access a storage slot that is associated with another
    /// sender's UserOp in the mempool.
    ///
    /// "Associated storage" means storage slots that were written during
    /// validation of another UserOp.
    fn check_sto041(&self, user_op: &PooledUserOp, pool: &EntryPointPool) -> MempoolResult<()> {
        // Check if any of our storage slots conflict with existing UserOps
        if !user_op.storage_slots.is_empty() {
            if let Some(conflicting_hash) = pool.has_storage_conflict(&user_op.storage_slots) {
                // Check if it's from a different sender
                if let Some(conflicting) = pool.get(&conflicting_hash) {
                    if conflicting.sender != user_op.sender {
                        return Err(MempoolError::rule_violation(
                            Erc7562Rule::Sto041AssociatedStorageConflict,
                            format!(
                                "Storage slot conflict with UserOp {} from sender {}",
                                conflicting_hash, conflicting.sender
                            ),
                            Some(conflicting_hash),
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// AUTH-040: Multiple UserOps from same sender must have same delegate
    ///
    /// If a sender uses EIP-7702 delegation, all UserOps from that sender
    /// in the mempool must have the same delegate address.
    ///
    /// This prevents front-running attacks where an attacker changes the
    /// delegate between UserOp validation and execution.
    fn check_auth040(&self, user_op: &PooledUserOp, pool: &EntryPointPool) -> MempoolResult<()> {
        // Get delegate from this UserOp's validation output
        let our_delegate = self.get_delegate(user_op);

        // If we don't have a delegate, nothing to check
        let Some(our_delegate) = our_delegate else {
            return Ok(());
        };

        // Check all existing UserOps from the same sender
        let existing = pool.get_by_sender(&user_op.sender);
        for existing_op in existing {
            if let Some(existing_delegate) = self.get_delegate(existing_op) {
                if existing_delegate != our_delegate {
                    return Err(MempoolError::rule_violation(
                        Erc7562Rule::Auth040DifferentDelegates,
                        format!(
                            "UserOp has delegate {} but existing UserOp {} has delegate {}",
                            our_delegate, existing_op.hash, existing_delegate
                        ),
                        Some(existing_op.hash),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Extract the EIP-7702 delegate address from a UserOp's validation
    ///
    /// TODO: This needs to be populated during validation by checking
    /// if the sender has the 0xef0100 delegation prefix.
    fn get_delegate(&self, _user_op: &PooledUserOp) -> Option<Address> {
        // TODO: Extract from validation trace or code analysis
        // For now, return None (no delegation detected)
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mempool::config::MempoolConfig;
    use crate::rpc::{UserOperation, UserOperationV07};
    use crate::simulation::{ReturnInfo, StakeInfo, ValidationOutput};
    use alloy_primitives::{Bytes, B256, U256};

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn test_user_op(sender: Address, nonce: u64) -> UserOperation {
        UserOperation::V07(UserOperationV07 {
            sender,
            nonce: U256::from(nonce),
            factory: Address::ZERO,
            factory_data: Bytes::default(),
            call_data: Bytes::default(),
            call_gas_limit: U256::from(100_000),
            verification_gas_limit: U256::from(100_000),
            pre_verification_gas: U256::from(50_000),
            max_fee_per_gas: U256::from(10_000_000_000u64),
            max_priority_fee_per_gas: U256::from(1_000_000_000u64),
            paymaster: Address::ZERO,
            paymaster_verification_gas_limit: U256::ZERO,
            paymaster_post_op_gas_limit: U256::ZERO,
            paymaster_data: Bytes::default(),
            signature: Bytes::default(),
        })
    }

    fn test_validation_output(sender: Address) -> ValidationOutput {
        ValidationOutput {
            valid: true,
            return_info: ReturnInfo {
                pre_op_gas: 100_000,
                prefund: U256::from(1_000_000_000_000_000u64),
                sig_failed: false,
                valid_after: 0,
                valid_until: 0,
                paymaster_context: Bytes::default(),
            },
            sender_info: StakeInfo {
                address: sender,
                stake: U256::ZERO,
                unstake_delay_sec: 0,
                deposit: U256::from(1_000_000_000_000_000u64),
                is_staked: false,
            },
            factory_info: None,
            paymaster_info: None,
            aggregator_info: None,
            code_hashes: None,
            violations: vec![],
            trace: None,
        }
    }

    fn create_pooled_op(
        sender: Address,
        nonce: u64,
        paymaster: Option<Address>,
        factory: Option<Address>,
    ) -> PooledUserOp {
        let config = MempoolConfig::default();
        let entry_point = test_address(1);
        let mut user_op = test_user_op(sender, nonce);

        // Set paymaster if provided
        if let UserOperation::V07(ref mut op) = user_op {
            if let Some(pm) = paymaster {
                op.paymaster = pm;
            }
            if let Some(f) = factory {
                op.factory = f;
            }
        }

        let hash = user_op.hash(entry_point, 1);
        let mut validation = test_validation_output(sender);

        // Set paymaster/factory info
        if let Some(pm) = paymaster {
            validation.paymaster_info = Some(StakeInfo {
                address: pm,
                stake: U256::ZERO,
                unstake_delay_sec: 0,
                deposit: U256::from(1_000_000_000_000_000u64),
                is_staked: false,
            });
        }
        if let Some(f) = factory {
            validation.factory_info = Some(StakeInfo {
                address: f,
                stake: U256::ZERO,
                unstake_delay_sec: 0,
                deposit: U256::from(1_000_000_000_000_000u64),
                is_staked: false,
            });
        }

        let mut pooled = PooledUserOp::new(user_op, hash, entry_point, validation, &config);
        pooled.paymaster = paymaster;
        pooled.factory = factory;
        pooled
    }

    #[test]
    fn test_sto040_entity_as_sender() {
        let entry_point = test_address(1);
        let config = MempoolConfig::default();
        let mut pool = EntryPointPool::new(entry_point, config.clone());
        let checker = MempoolRuleChecker::new();

        let sender1 = test_address(10);
        let sender2 = test_address(11);

        // Add a UserOp from sender1
        let op1 = create_pooled_op(sender1, 0, None, None);
        pool.add(op1).unwrap();

        // Try to add a UserOp that uses sender1 as paymaster
        // This should fail because sender1 is already a sender in the pool
        let op2 = create_pooled_op(sender2, 0, Some(sender1), None);
        let result = checker.check(&op2, &pool);

        assert!(matches!(
            result,
            Err(MempoolError::MempoolRuleViolation {
                rule: Erc7562Rule::Sto040EntityUsedAsAccount,
                ..
            })
        ));
    }

    #[test]
    fn test_sto040_different_sender_ok() {
        let entry_point = test_address(1);
        let config = MempoolConfig::default();
        let mut pool = EntryPointPool::new(entry_point, config.clone());
        let checker = MempoolRuleChecker::new();

        let sender1 = test_address(10);
        let sender2 = test_address(11);
        let paymaster = test_address(20);

        // Add a UserOp from sender1 with a paymaster
        let op1 = create_pooled_op(sender1, 0, Some(paymaster), None);
        pool.add(op1).unwrap();

        // Add a UserOp from sender2 with the same paymaster
        // This should succeed (paymaster isn't a sender)
        let op2 = create_pooled_op(sender2, 0, Some(paymaster), None);
        let result = checker.check(&op2, &pool);

        assert!(result.is_ok());
    }

    #[test]
    fn test_sto041_storage_conflict() {
        let entry_point = test_address(1);
        let config = MempoolConfig::default();
        let mut pool = EntryPointPool::new(entry_point, config.clone());
        let checker = MempoolRuleChecker::new();

        let sender1 = test_address(10);
        let sender2 = test_address(11);
        let contract = test_address(30);
        let slot = B256::from([1u8; 32]);

        // Add a UserOp from sender1 that accesses a storage slot
        let mut op1 = create_pooled_op(sender1, 0, None, None);
        op1.storage_slots.insert((contract, slot));
        pool.add(op1).unwrap();

        // Try to add a UserOp from sender2 that accesses the same slot
        let mut op2 = create_pooled_op(sender2, 0, None, None);
        op2.storage_slots.insert((contract, slot));
        let result = checker.check(&op2, &pool);

        assert!(matches!(
            result,
            Err(MempoolError::MempoolRuleViolation {
                rule: Erc7562Rule::Sto041AssociatedStorageConflict,
                ..
            })
        ));
    }

    #[test]
    fn test_sto041_same_sender_ok() {
        let entry_point = test_address(1);
        let config = MempoolConfig::default();
        let mut pool = EntryPointPool::new(entry_point, config.clone());
        let checker = MempoolRuleChecker::new();

        let sender = test_address(10);
        let contract = test_address(30);
        let slot = B256::from([1u8; 32]);

        // Add a UserOp from sender that accesses a storage slot
        let mut op1 = create_pooled_op(sender, 0, None, None);
        op1.storage_slots.insert((contract, slot));
        pool.add(op1).unwrap();

        // Add another UserOp from the same sender with the same slot
        // This should succeed (same sender is allowed)
        let mut op2 = create_pooled_op(sender, 1, None, None);
        op2.storage_slots.insert((contract, slot));
        let result = checker.check(&op2, &pool);

        assert!(result.is_ok());
    }

    #[test]
    fn test_no_conflicts_empty_pool() {
        let entry_point = test_address(1);
        let config = MempoolConfig::default();
        let pool = EntryPointPool::new(entry_point, config.clone());
        let checker = MempoolRuleChecker::new();

        let sender = test_address(10);
        let op = create_pooled_op(sender, 0, None, None);

        let result = checker.check(&op, &pool);
        assert!(result.is_ok());
    }
}
