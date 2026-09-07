use crate::{Evm, EvmEnv};
use alloy_primitives::{Address, Bytes};
use revm::context::{either, CfgEnv};

impl<L, R> Evm for either::Either<L, R>
where
    L: Evm,
    R: Evm<
        DB = L::DB,
        Tx = L::Tx,
        Error = L::Error,
        HaltReason = L::HaltReason,
        Spec = L::Spec,
        BlockEnv = L::BlockEnv,
        Precompiles = L::Precompiles,
        Inspector = L::Inspector,
    >,
{
    type DB = L::DB;
    type Tx = L::Tx;
    type Error = L::Error;
    type HaltReason = L::HaltReason;
    type Spec = L::Spec;
    type BlockEnv = L::BlockEnv;
    type Precompiles = L::Precompiles;
    type Inspector = L::Inspector;

    fn block(&self) -> &Self::BlockEnv {
        either::for_both!(self, evm => evm.block())
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        either::for_both!(self, evm => evm.cfg_env())
    }

    fn chain_id(&self) -> u64 {
        either::for_both!(self, evm => evm.chain_id())
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<revm::context::result::ResultAndState<Self::HaltReason>, Self::Error> {
        either::for_both!(self, evm => evm.transact_raw(tx))
    }

    fn transact(
        &mut self,
        tx: impl crate::IntoTxEnv<Self::Tx>,
    ) -> Result<revm::context::result::ResultAndState<Self::HaltReason>, Self::Error> {
        either::for_both!(self, evm => evm.transact(tx))
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<revm::context::result::ResultAndState<Self::HaltReason>, Self::Error> {
        either::for_both!(self, evm => evm.transact_system_call(caller, contract, data))
    }

    fn transact_commit(
        &mut self,
        tx: impl crate::IntoTxEnv<Self::Tx>,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error>
    where
        Self::DB: revm::DatabaseCommit,
    {
        either::for_both!(self, evm => evm.transact_commit(tx))
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>)
    where
        Self: Sized,
    {
        either::for_both!(self, evm => evm.finish())
    }

    fn into_db(self) -> Self::DB
    where
        Self: Sized,
    {
        either::for_both!(self, evm => evm.into_db())
    }

    fn into_env(self) -> EvmEnv<Self::Spec, Self::BlockEnv>
    where
        Self: Sized,
    {
        either::for_both!(self, evm => evm.into_env())
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        either::for_both!(self, evm => evm.set_inspector_enabled(enabled))
    }

    fn enable_inspector(&mut self) {
        either::for_both!(self, evm => evm.enable_inspector())
    }

    fn disable_inspector(&mut self) {
        either::for_both!(self, evm => evm.disable_inspector())
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        either::for_both!(self, evm => evm.components())
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        either::for_both!(self, evm => evm.components_mut())
    }
}
