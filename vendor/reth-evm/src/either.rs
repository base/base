//! Helper type that represents one of two possible executor types

// re-export Either
use base_common_consensus::{BaseBlock, BaseReceipt};
pub use futures_util::future::Either;
use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult};
use reth_primitives_traits::RecoveredBlock;

use crate::{Database, OnStateHook, execute::Executor};

impl<A, B, DB> Executor<DB> for Either<A, B>
where
    A: Executor<DB>,
    B: Executor<DB, Error = A::Error>,
    DB: Database,
{
    type Error = A::Error;

    fn execute_one(
        &mut self,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error> {
        match self {
            Self::Left(a) => a.execute_one(block),
            Self::Right(b) => b.execute_one(block),
        }
    }

    fn execute_one_with_state_hook<F>(
        &mut self,
        block: &RecoveredBlock<BaseBlock>,
        state_hook: F,
    ) -> Result<BlockExecutionResult<BaseReceipt>, Self::Error>
    where
        F: OnStateHook + 'static,
    {
        match self {
            Self::Left(a) => a.execute_one_with_state_hook(block, state_hook),
            Self::Right(b) => b.execute_one_with_state_hook(block, state_hook),
        }
    }

    fn execute(
        self,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<BlockExecutionOutput<BaseReceipt>, Self::Error> {
        match self {
            Self::Left(a) => a.execute(block),
            Self::Right(b) => b.execute(block),
        }
    }

    fn execute_with_state_closure<F>(
        self,
        block: &RecoveredBlock<BaseBlock>,
        state: F,
    ) -> Result<BlockExecutionOutput<BaseReceipt>, Self::Error>
    where
        F: FnMut(&revm::database::State<DB>),
    {
        match self {
            Self::Left(a) => a.execute_with_state_closure(block, state),
            Self::Right(b) => b.execute_with_state_closure(block, state),
        }
    }

    fn into_state(self) -> revm::database::State<DB> {
        match self {
            Self::Left(a) => a.into_state(),
            Self::Right(b) => b.into_state(),
        }
    }

    fn size_hint(&self) -> usize {
        match self {
            Self::Left(a) => a.size_hint(),
            Self::Right(b) => b.size_hint(),
        }
    }

    fn take_bal(&mut self) -> Option<alloy_eip7928::BlockAccessList> {
        match self {
            Self::Left(a) => a.take_bal(),
            Self::Right(b) => b.take_bal(),
        }
    }
}
