use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
};

use alloy_primitives::Address;
use tracing::warn;

use crate::domain::{
    mempool::{Mempool, PoolConfig},
    types::{UserOpHash, WrappedUserOperation},
};

#[derive(Eq, PartialEq, Clone, Debug)]
struct OrderedPoolOperation {
    pool_operation: WrappedUserOperation,
    submission_id: u64,
}

impl OrderedPoolOperation {
    fn from_wrapped(operation: &WrappedUserOperation, submission_id: u64) -> Self {
        Self { pool_operation: operation.clone(), submission_id }
    }

    fn sender(&self) -> Address {
        self.pool_operation.operation.sender()
    }
}

#[derive(Clone, Debug)]
struct ByMaxFeeAndSubmissionId(OrderedPoolOperation);

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
    }
}

#[derive(Clone, Debug)]
struct ByNonce(OrderedPoolOperation);

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
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .pool_operation
            .operation
            .nonce()
            .cmp(&other.0.pool_operation.operation.nonce())
            .then_with(|| self.0.submission_id.cmp(&other.0.submission_id))
            .then_with(|| self.0.pool_operation.hash.cmp(&other.0.pool_operation.hash))
    }
}

pub struct InMemoryMempool {
    config: PoolConfig,
    best: BTreeSet<ByMaxFeeAndSubmissionId>,
    hash_to_operation: HashMap<UserOpHash, OrderedPoolOperation>,
    operations_by_account: HashMap<Address, BTreeSet<ByNonce>>,
    submission_id_counter: AtomicU64,
}

impl Mempool for InMemoryMempool {
    fn add_operation(&mut self, operation: &WrappedUserOperation) -> Result<(), anyhow::Error> {
        if operation.operation.max_fee_per_gas() < self.config.minimum_max_fee_per_gas {
            return Err(anyhow::anyhow!("Gas price is below the minimum required PVG gas"));
        }
        self.handle_add_operation(operation)?;
        Ok(())
    }

    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>> {
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
                        warn!(
                            account = %op_by_fee.0.sender(),
                            "Inconsistent state: operation in best set but not in account index"
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
            self.best.remove(&ByMaxFeeAndSubmissionId(ordered_operation.clone()));
            self.operations_by_account
                .get_mut(&ordered_operation.sender())
                .map(|set| set.remove(&ByNonce(ordered_operation.clone())));
            Ok(Some(ordered_operation.pool_operation))
        } else {
            Ok(None)
        }
    }
}

impl InMemoryMempool {
    fn handle_add_operation(
        &mut self,
        operation: &WrappedUserOperation,
    ) -> Result<(), anyhow::Error> {
        if self.hash_to_operation.contains_key(&operation.hash) {
            return Ok(());
        }

        let order = self.get_next_order_id();
        let ordered_operation = OrderedPoolOperation::from_wrapped(operation, order);

        self.best.insert(ByMaxFeeAndSubmissionId(ordered_operation.clone()));
        self.operations_by_account
            .entry(ordered_operation.sender())
            .or_default()
            .insert(ByNonce(ordered_operation.clone()));
        self.hash_to_operation.insert(operation.hash, ordered_operation.clone());
        Ok(())
    }

    fn get_next_order_id(&self) -> u64 {
        self.submission_id_counter.fetch_add(1, AtomicOrdering::SeqCst)
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
    use alloy_primitives::{Address, FixedBytes, Uint};
    use alloy_rpc_types::erc4337;

    use super::*;
    use crate::domain::types::VersionedUserOperation;

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

    fn create_test_mempool(minimum_required_pvg_gas: u128) -> InMemoryMempool {
        InMemoryMempool::new(PoolConfig { minimum_max_fee_per_gas: minimum_required_pvg_gas })
    }

    #[test]
    fn test_add_operation_success() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);
        let operation = create_wrapped_operation(2000, hash);

        let result = mempool.add_operation(&operation);

        assert!(result.is_ok());
    }

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

    #[test]
    fn test_add_multiple_operations_with_different_hashes() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_wrapped_operation(2000, hash1);
        let result1 = mempool.add_operation(&operation1);
        assert!(result1.is_ok());

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_wrapped_operation(3000, hash2);
        let result2 = mempool.add_operation(&operation2);
        assert!(result2.is_ok());

        let hash3 = FixedBytes::from([3u8; 32]);
        let operation3 = create_wrapped_operation(1500, hash3);
        let result3 = mempool.add_operation(&operation3);
        assert!(result3.is_ok());

        assert_eq!(mempool.hash_to_operation.len(), 3);
        assert_eq!(mempool.best.len(), 3);
    }

    #[test]
    fn test_remove_operation_not_in_mempool() {
        let mut mempool = create_test_mempool(1000);
        let hash = FixedBytes::from([1u8; 32]);

        let result = mempool.remove_operation(&hash);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

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

    #[test]
    fn test_get_top_operations_submission_id_tie_breaker() {
        let mut mempool = create_test_mempool(1000);

        let hash1 = FixedBytes::from([1u8; 32]);
        let operation1 = create_wrapped_operation(2000, hash1);
        mempool.add_operation(&operation1).unwrap();

        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = create_wrapped_operation(2000, hash2);
        mempool.add_operation(&operation2).unwrap();

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

        mempool.add_operation(&operation1).unwrap();
        let hash2 = FixedBytes::from([2u8; 32]);
        let operation2 = WrappedUserOperation {
            operation: VersionedUserOperation::UserOperation(erc4337::UserOperation {
                nonce: Uint::from(1),
                max_fee_per_gas: Uint::from(10_000),
                ..base_op.clone()
            }),
            hash: hash2,
        };
        mempool.add_operation(&operation2).unwrap();

        let best: Vec<_> = mempool.get_top_operations(2).collect();
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].operation.nonce(), Uint::from(0));
    }
}
