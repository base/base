//! ABI dispatch for the EIP-8130 transaction context precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    ITransactionContext::{self, ITransactionContextCalls as C},
    macros::decode_precompile_call,
    tx_context::storage::TxContextStorage,
};

/// Per-word calldata gas charge (`G_SHA3WORD`), matching common Base precompile dispatch.
const CALLDATA_WORD_GAS: u64 = 6;

impl TxContextStorage<'_> {
    /// ABI-dispatches transaction context calldata.
    pub fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        let calldata_cost = (calldata.len() as u64).div_ceil(32).saturating_mul(CALLDATA_WORD_GAS);
        if let Err(error) = ctx.deduct_gas(calldata_cost) {
            return error.into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        }
        // These getters never produce a gas refund, so the refund arg is 0.
        self.inner(calldata).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            0,
            |output| output,
        )
    }

    fn inner(&self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, ITransactionContext::ITransactionContextCalls) {
            C::getTransactionSender(_) => Ok(
                ITransactionContext::getTransactionSenderCall::abi_encode_returns(&self.sender()?)
                    .into(),
            ),
            C::getTransactionPayer(_) => {
                Ok(ITransactionContext::getTransactionPayerCall::abi_encode_returns(&self.payer()?)
                    .into())
            }
            C::getTransactionSenderActorId(_) => {
                Ok(ITransactionContext::getTransactionSenderActorIdCall::abi_encode_returns(
                    &self.sender_actor_id()?,
                )
                .into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, address, b256};
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{ITransactionContext, TxContextStorage};

    const SENDER: Address = address!("0x1111111111111111111111111111111111111111");
    const PAYER: Address = address!("0x2222222222222222222222222222222222222222");
    const SENDER_ACTOR_ID: B256 =
        b256!("0x3333333333333333333333333333333333333333333333333333333333333333");
    const ORIGIN: Address = address!("0x9999999999999999999999999999999999999999");

    fn dispatch(storage: &mut HashMapStorageProvider, calldata: &[u8]) -> Vec<u8> {
        StorageCtx::enter(storage, |ctx| {
            TxContextStorage::new(ctx)
                .dispatch(ctx, calldata)
                .expect("dispatch should not fail fatally")
                .bytes
                .to_vec()
        })
    }

    #[test]
    fn dispatch_returns_resolved_context() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).set_context(SENDER, PAYER, SENDER_ACTOR_ID).unwrap();
        });

        let sender =
            dispatch(&mut storage, &ITransactionContext::getTransactionSenderCall {}.abi_encode());
        assert_eq!(
            ITransactionContext::getTransactionSenderCall::abi_decode_returns(&sender).unwrap(),
            SENDER
        );

        let payer =
            dispatch(&mut storage, &ITransactionContext::getTransactionPayerCall {}.abi_encode());
        assert_eq!(
            ITransactionContext::getTransactionPayerCall::abi_decode_returns(&payer).unwrap(),
            PAYER
        );

        let actor_id = dispatch(
            &mut storage,
            &ITransactionContext::getTransactionSenderActorIdCall {}.abi_encode(),
        );
        assert_eq!(
            ITransactionContext::getTransactionSenderActorIdCall::abi_decode_returns(&actor_id)
                .unwrap(),
            SENDER_ACTOR_ID
        );
    }

    #[test]
    fn dispatch_falls_back_to_origin_when_unset() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        let sender =
            dispatch(&mut storage, &ITransactionContext::getTransactionSenderCall {}.abi_encode());
        assert_eq!(
            ITransactionContext::getTransactionSenderCall::abi_decode_returns(&sender).unwrap(),
            ORIGIN
        );

        let payer =
            dispatch(&mut storage, &ITransactionContext::getTransactionPayerCall {}.abi_encode());
        assert_eq!(
            ITransactionContext::getTransactionPayerCall::abi_decode_returns(&payer).unwrap(),
            ORIGIN
        );
    }

    #[test]
    fn dispatch_reverts_on_unknown_selector() {
        let mut storage = HashMapStorageProvider::new(1);
        let output = StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).dispatch(ctx, &[0xde, 0xad, 0xbe, 0xef])
        })
        .expect("unknown selector should revert, not fail fatally");

        assert!(output.is_revert());
    }
}
