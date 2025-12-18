use crate::types::{PoolOperation, UserOpHash};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

pub struct PoolConfig {
    minimum_max_fee_per_gas: u128,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct OrderedPoolOperation {
    pub pool_operation: PoolOperation,
    pub submission_id: u64,
    pub priority_order: u64,
}

impl Ord for OrderedPoolOperation {
    /// TODO: There can be invalid opperations, where base fee, + expected gas price
    /// is greater that the maximum gas, in that case we don't include it in the mempool as such mempool changes.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .pool_operation
            .operation
            .max_priority_fee_per_gas()
            .cmp(&self.pool_operation.operation.max_priority_fee_per_gas())
            .then_with(|| self.submission_id.cmp(&other.submission_id))
    }
}

impl PartialOrd for OrderedPoolOperation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl OrderedPoolOperation {
    pub fn create_from_pool_operation(operation: &PoolOperation, submission_id: u64) -> Self {
        Self {
            pool_operation: operation.clone(),
            priority_order: submission_id,
            submission_id,
        }
    }
}

pub trait Mempool {
    fn add_operation(
        &mut self,
        operation: &PoolOperation,
    ) -> Result<Option<OrderedPoolOperation>, anyhow::Error>;
    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<PoolOperation>>;
    fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<PoolOperation>, anyhow::Error>;
}

pub struct MempoolImpl {
    config: PoolConfig,
    best: BTreeSet<OrderedPoolOperation>,
    hash_to_operation: HashMap<UserOpHash, OrderedPoolOperation>,
    submission_id_counter: AtomicU64,
}

impl Mempool for MempoolImpl {
    fn add_operation(
        &mut self,
        operation: &PoolOperation,
    ) -> Result<Option<OrderedPoolOperation>, anyhow::Error> {
        if operation.operation.max_fee_per_gas() < self.config.minimum_max_fee_per_gas {
            return Err(anyhow::anyhow!(
                "Gas price is below the minimum required PVG gas"
            ));
        }
        let ordered_operation_result = self.handle_add_operation(operation)?;
        Ok(ordered_operation_result)
    }

    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<PoolOperation>> {
        self.best
            .iter()
            .take(n)
            .map(|o| Arc::new(o.pool_operation.clone()))
    }

    fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<PoolOperation>, anyhow::Error> {
        if let Some(ordered_operation) = self.hash_to_operation.remove(operation_hash) {
            self.best.remove(&ordered_operation);
            Ok(Some(ordered_operation.pool_operation))
        } else {
            Ok(None)
        }
    }
}

impl MempoolImpl {
    fn handle_add_operation(
        &mut self,
        operation: &PoolOperation,
    ) -> Result<Option<OrderedPoolOperation>, anyhow::Error> {
        if let Some(old_ordered_operation) = self.hash_to_operation.get(&operation.hash) {
            if operation.should_replace(&old_ordered_operation.pool_operation) {
                self.best.remove(old_ordered_operation);
                self.hash_to_operation.remove(&operation.hash);
            } else {
                return Ok(None);
            }
        }

        let order = self.get_next_order_id();
        let ordered_operation = OrderedPoolOperation::create_from_pool_operation(operation, order);

        self.best.insert(ordered_operation.clone());
        self.hash_to_operation
            .insert(operation.hash, ordered_operation.clone());
        Ok(Some(ordered_operation))
    }

    fn get_next_order_id(&self) -> u64 {
        self.submission_id_counter.fetch_add(1, Ordering::SeqCst)
    }

    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            best: BTreeSet::new(),
            hash_to_operation: HashMap::new(),
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
            sender: Address::ZERO,
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

    fn create_pool_operation(max_priority_fee_per_gas: u128, hash: UserOpHash) -> PoolOperation {
        PoolOperation {
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
        let operation = create_pool_operation(2000, hash);

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
        let operation = create_pool_operation(1000, hash);

        let result = mempool.add_operation(&operation);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Gas price is below the minimum required PVG gas")
        );
    }

    // Tests adding an operation with the same hash but higher gas price
    #[test]
    fn test_add_operation_duplicate_hash_higher_gas() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);

        let operation1 = create_pool_operation(2000, hash);
        let result1 = mempool.add_operation(&operation1);
        assert!(result1.is_ok());
        assert!(result1.unwrap().is_some());

        let operation2 = create_pool_operation(3000, hash);
        let result2 = mempool.add_operation(&operation2);
        assert!(result2.is_ok());
        assert!(result2.unwrap().is_some());
    }

    // Tests adding an operation with the same hash but lower gas price
    #[test]
    fn test_add_operation_duplicate_hash_lower_gas() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);

        let operation1 = create_pool_operation(3000, hash);
        let result1 = mempool.add_operation(&operation1);
        assert!(result1.is_ok());
        assert!(result1.unwrap().is_some());

        let operation2 = create_pool_operation(2000, hash);
        let result2 = mempool.add_operation(&operation2);
        assert!(result2.is_ok());
        assert!(result2.unwrap().is_none());
    }

    // Tests adding an operation with the same hash and equal gas price
    #[test]
    fn test_add_operation_duplicate_hash_equal_gas() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);

        let operation1 = create_pool_operation(2000, hash);
        let result1 = mempool.add_operation(&operation1);
        assert!(result1.is_ok());
        assert!(result1.unwrap().is_some());

        let operation2 = create_pool_operation(2000, hash);
        let result2 = mempool.add_operation(&operation2);
        assert!(result2.is_ok());
        assert!(result2.unwrap().is_none());
    }

    // Tests adding multiple operations with different hashes
    #[test]
    fn test_add_multiple_operations_with_different_hashes() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_pool_operation(2000, hash1);
        let result1 = mempool.add_operation(&operation1);
        assert!(result1.is_ok());
        assert!(result1.unwrap().is_some());

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_pool_operation(3000, hash2);
        let result2 = mempool.add_operation(&operation2);
        assert!(result2.is_ok());
        assert!(result2.unwrap().is_some());

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_pool_operation(1500, hash3);
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
        let operation = create_pool_operation(2000, hash);

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
        let operation = create_pool_operation(2000, hash);

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
        let operation1 = create_pool_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_pool_operation(3000, hash2);
        mempool.add_operation(&operation2).unwrap();

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_pool_operation(1500, hash3);
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
        let operation1 = create_pool_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_pool_operation(3000, hash2);
        mempool.add_operation(&operation2).unwrap();

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_pool_operation(1500, hash3);
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
        let operation1 = create_pool_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap().unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_pool_operation(2000, hash2);
        mempool.add_operation(&operation2).unwrap().unwrap();

        let best: Vec<_> = mempool.get_top_operations(2).collect();
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].hash, hash1);
        assert_eq!(best[1].hash, hash2);
    }
}
