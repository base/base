use crate::domain::types::{UserOpHash, WrappedUserOperation};
use std::sync::Arc;

#[derive(Default)]
pub struct PoolConfig {
    pub minimum_max_fee_per_gas: u128,
}

pub trait Mempool: Send + Sync {
    fn add_operation(&mut self, operation: &WrappedUserOperation) -> Result<(), anyhow::Error>;

    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>>;

    fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<WrappedUserOperation>, anyhow::Error>;
}
