use crate::types::{UserOpHash, WrappedUserOperation};
use alloy_primitives::Address;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub struct PoolConfig {
    minimum_max_fee_per_gas: u128,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct OrderedPoolOperation {
    pub pool_operation: WrappedUserOperation,
    pub submission_id: u64,
}

impl OrderedPoolOperation {
    pub fn from_wrapped(operation: &WrappedUserOperation, submission_id: u64) -> Self {
        Self {
            pool_operation: operation.clone(),
            submission_id,
        }
    }

    pub fn sender(&self) -> Address {
        self.pool_operation.operation.sender()
    }
}

/// Ordering by max priority fee (desc) then submission id, then hash to ensure total order
#[derive(Clone, Debug)]
pub struct ByMaxFeeAndSubmissionId(pub OrderedPoolOperation);

impl PartialEq for ByMaxFeeAndSubmissionId {
    fn eq(&self, other: &Self) -> bool {
        self.0.pool_operation.hash == other.0.pool_operation.hash
    }
}
impl Eq for ByMaxFeeAndSubmissionId {}

impl PartialOrd for ByMaxFeeAndSubmissionId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByMaxFeeAndSubmissionId {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .pool_operation
            .operation
            .max_priority_fee_per_gas()
            .cmp(&self.0.pool_operation.operation.max_priority_fee_per_gas())
            .then_with(|| self.0.submission_id.cmp(&other.0.submission_id))
            .then_with(|| self.0.pool_operation.hash.cmp(&other.0.pool_operation.hash))
    }
}

/// Ordering by nonce (asc), then submission id, then hash to ensure total order
#[derive(Clone, Debug)]
pub struct ByNonce(pub OrderedPoolOperation);

impl PartialEq for ByNonce {
    fn eq(&self, other: &Self) -> bool {
        self.0.pool_operation.hash == other.0.pool_operation.hash
    }
}
impl Eq for ByNonce {}

impl PartialOrd for ByNonce {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByNonce {
    /// TODO: There can be invalid opperations, where base fee, + expected gas price
    /// is greater that the maximum gas, in that case we don't include it in the mempool as such mempool changes.
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .pool_operation
            .operation
            .nonce()
            .cmp(&other.0.pool_operation.operation.nonce())
            .then_with(|| self.0.submission_id.cmp(&other.0.submission_id))
    }
}

pub trait Mempool {
    fn add_operation(
        &mut self,
        operation: &WrappedUserOperation,
    ) -> Result<Option<OrderedPoolOperation>, anyhow::Error>;
    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>>;
    fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<WrappedUserOperation>, anyhow::Error>;
}

pub struct MempoolImpl {
    config: PoolConfig,
    best: BTreeSet<ByMaxFeeAndSubmissionId>,
    hash_to_operation: HashMap<UserOpHash, OrderedPoolOperation>,
    operations_by_account: HashMap<Address, BTreeSet<ByNonce>>,
    submission_id_counter: AtomicU64,
}

impl Mempool for MempoolImpl {
    fn add_operation(
        &mut self,
        operation: &WrappedUserOperation,
    ) -> Result<Option<OrderedPoolOperation>, anyhow::Error> {
        if operation.operation.max_fee_per_gas() < self.config.minimum_max_fee_per_gas {
            return Err(anyhow::anyhow!(
                "Gas price is below the minimum required PVG gas"
            ));
        }
        let ordered_operation_result = self.handle_add_operation(operation)?;
        Ok(ordered_operation_result)
    }

    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>> {
        // TODO: There is a case where we skip operations that are not the lowest nonce for an account.
        // But we still have not given the N number of operations, meaning we don't return those operations.

        self.best
            .iter()
            .filter_map(|op_by_fee| {
                let lowest = self
                    .operations_by_account
                    .get(&op_by_fee.0.sender())
                    .and_then(|set| set.first());

                match lowest {
                    Some(lowest)
                        if lowest.0.pool_operation.hash == op_by_fee.0.pool_operation.hash =>
                    {
                        Some(Arc::new(op_by_fee.0.pool_operation.clone()))
                    }
                    Some(_) => None,
                    None => {
                        println!(
                            "No operations found for account: {} but one was found in the best set",
                            op_by_fee.0.sender()
                        );
                        None
                    }
                }
            })
            .take(n)
    }

    fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<WrappedUserOperation>, anyhow::Error> {
        if let Some(ordered_operation) = self.hash_to_operation.remove(operation_hash) {
            self.best
                .remove(&ByMaxFeeAndSubmissionId(ordered_operation.clone()));
            self.operations_by_account
                .get_mut(&ordered_operation.sender())
                .map(|set| set.remove(&ByNonce(ordered_operation.clone())));
            Ok(Some(ordered_operation.pool_operation))
        } else {
            Ok(None)
        }
    }
}

// When user opperation is added to the mempool we need to check

impl MempoolImpl {
    fn handle_add_operation(
        &mut self,
        operation: &WrappedUserOperation,
    ) -> Result<Option<OrderedPoolOperation>, anyhow::Error> {
        // Account
        if self.hash_to_operation.contains_key(&operation.hash) {
            return Ok(None);
        }

        let order = self.get_next_order_id();
        let ordered_operation = OrderedPoolOperation::from_wrapped(operation, order);

        self.best
            .insert(ByMaxFeeAndSubmissionId(ordered_operation.clone()));
        self.operations_by_account
            .entry(ordered_operation.sender())
            .or_default()
            .insert(ByNonce(ordered_operation.clone()));
        self.hash_to_operation
            .insert(operation.hash, ordered_operation.clone());
        Ok(Some(ordered_operation))
    }

    fn get_next_order_id(&self) -> u64 {
        self.submission_id_counter
            .fetch_add(1, AtomicOrdering::SeqCst)
    }

    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            best: BTreeSet::new(),
            hash_to_operation: HashMap::new(),
            operations_by_account: HashMap::new(),
            submission_id_counter: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VersionedUserOperation;
    use alloy_primitives::{Address, FixedBytes, Uint};
    use alloy_rpc_types::erc4337;
    fn create_test_user_operation(max_priority_fee_per_gas: u128) -> VersionedUserOperation {
        VersionedUserOperation::UserOperation(erc4337::UserOperation {
            sender: Address::random(),
            nonce: Uint::from(0),
            init_code: Default::default(),
            call_data: Default::default(),
            call_gas_limit: Uint::from(100000),
            verification_gas_limit: Uint::from(100000),
            pre_verification_gas: Uint::from(21000),
            max_fee_per_gas: Uint::from(max_priority_fee_per_gas),
            max_priority_fee_per_gas: Uint::from(max_priority_fee_per_gas),
            paymaster_and_data: Default::default(),
            signature: Default::default(),
        })
    }

    fn create_wrapped_operation(
        max_priority_fee_per_gas: u128,
        hash: UserOpHash,
    ) -> WrappedUserOperation {
        WrappedUserOperation {
            operation: create_test_user_operation(max_priority_fee_per_gas),
            hash,
        }
    }

    fn create_test_mempool(minimum_required_pvg_gas: u128) -> MempoolImpl {
        MempoolImpl::new(PoolConfig {
            minimum_max_fee_per_gas: minimum_required_pvg_gas,
        })
    }

    // Tests successfully adding a valid operation to the mempool
    #[test]
    fn test_add_operation_success() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);
        let operation = create_wrapped_operation(2000, hash);

        let result = mempool.add_operation(&operation);

        assert!(result.is_ok());
        let ordered_op = result.unwrap();
        assert!(ordered_op.is_some());
        let ordered_op = ordered_op.unwrap();
        assert_eq!(ordered_op.pool_operation.hash, hash);
        assert_eq!(
            ordered_op.pool_operation.operation.max_fee_per_gas(),
            Uint::from(2000)
        );
    }

    // Tests adding an operation with a gas price below the minimum required PVG gas
    #[test]
    fn test_add_operation_below_minimum_gas() {
        let mut mempool = create_test_mempool(2000);
        let hash = FixedBytes::from([1u8; 32]);
        let operation = create_wrapped_operation(1000, hash);

        let result = mempool.add_operation(&operation);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Gas price is below the minimum required PVG gas")
        );
    }

    // Tests adding multiple operations with different hashes
    #[test]
    fn test_add_multiple_operations() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_wrapped_operation(2000, hash1);
        let result1 = mempool.add_operation(&operation1);
        assert!(result1.is_ok());
        assert!(result1.unwrap().is_some());

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_wrapped_operation(3000, hash2);
        let result2 = mempool.add_operation(&operation2);
        assert!(result2.is_ok());
        assert!(result2.unwrap().is_some());

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_wrapped_operation(1500, hash3);
        let result3 = mempool.add_operation(&operation3);
        assert!(result3.is_ok());
        assert!(result3.unwrap().is_some());

        assert_eq!(mempool.hash_to_operation.len(), 3);
        assert_eq!(mempool.best.len(), 3);
    }

    // Tests removing an operation that is not in the mempool
    #[test]
    fn test_remove_operation_not_in_mempool() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);

        let result = mempool.remove_operation(&hash);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // Tests removing an operation that exists in the mempool
    #[test]
    fn test_remove_operation_exists() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);
        let operation = create_wrapped_operation(2000, hash);

        mempool.add_operation(&operation).unwrap();

        let result = mempool.remove_operation(&hash);
        assert!(result.is_ok());
        let removed = result.unwrap();
        assert!(removed.is_some());
        let removed_op = removed.unwrap();
        assert_eq!(removed_op.hash, hash);
        assert_eq!(removed_op.operation.max_fee_per_gas(), Uint::from(2000));
    }

    // Tests removing an operation and checking the best operations
    #[test]
    fn test_remove_operation_and_check_best() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);
        let operation = create_wrapped_operation(2000, hash);

        mempool.add_operation(&operation).unwrap();

        let best_before: Vec<_> = mempool.get_top_operations(10).collect();
        assert_eq!(best_before.len(), 1);
        assert_eq!(best_before[0].hash, hash);

        let result = mempool.remove_operation(&hash);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());

        let best_after: Vec<_> = mempool.get_top_operations(10).collect();
        assert_eq!(best_after.len(), 0);
    }

    // Tests getting the top operations with ordering
    #[test]
    fn test_get_top_operations_ordering() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_wrapped_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_wrapped_operation(3000, hash2);
        mempool.add_operation(&operation2).unwrap();

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_wrapped_operation(1500, hash3);
        mempool.add_operation(&operation3).unwrap();

        let best: Vec<_> = mempool.get_top_operations(10).collect();
        assert_eq!(best.len(), 3);
        assert_eq!(best[0].operation.max_fee_per_gas(), Uint::from(3000));
        assert_eq!(best[1].operation.max_fee_per_gas(), Uint::from(2000));
        assert_eq!(best[2].operation.max_fee_per_gas(), Uint::from(1500));
    }

    // Tests getting the top operations with a limit
    #[test]
    fn test_get_top_operations_limit() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_wrapped_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_wrapped_operation(3000, hash2);
        mempool.add_operation(&operation2).unwrap();

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_wrapped_operation(1500, hash3);
        mempool.add_operation(&operation3).unwrap();

        let best: Vec<_> = mempool.get_top_operations(2).collect();
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].operation.max_fee_per_gas(), Uint::from(3000));
        assert_eq!(best[1].operation.max_fee_per_gas(), Uint::from(2000));
    }

    // Tests top opperations tie breaker with submission id
    #[test]
    fn test_get_top_operations_submission_id_tie_breaker() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_wrapped_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap().unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_wrapped_operation(2000, hash2);
        mempool.add_operation(&operation2).unwrap().unwrap();

        let best: Vec<_> = mempool.get_top_operations(2).collect();
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].hash, hash1);
        assert_eq!(best[1].hash, hash2);
    }

    #[test]
    fn test_get_top_operations_should_return_the_lowest_nonce_operation_for_each_account() {
        let mut mempool = create_test_mempool(1000);
        let hash1 = FixedBytes::from([1u8; 32]);
        let test_user_operation = create_test_user_operation(2000);

        // Destructure to the inner struct, then update nonce
        let base_op = match test_user_operation.clone() {
            VersionedUserOperation::UserOperation(op) => op,
            _ => panic!("expected UserOperation variant"),
        };

        let operation1 = WrappedUserOperation {
            operation: VersionedUserOperation::UserOperation(erc4337::UserOperation {
                nonce: Uint::from(0),
                max_fee_per_gas: Uint::from(2000),
                ..base_op.clone()
            }),
            hash: hash1,
        };

        mempool.add_operation(&operation1).unwrap().unwrap();
        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = WrappedUserOperation {
            operation: VersionedUserOperation::UserOperation(erc4337::UserOperation {
                nonce: Uint::from(1),
                max_fee_per_gas: Uint::from(10_000),
                ..base_op.clone()
            }),
            hash: hash2,
        };
        mempool.add_operation(&operation2).unwrap().unwrap();

        let best: Vec<_> = mempool.get_top_operations(2).collect();
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].operation.nonce(), Uint::from(0));
    }
}
