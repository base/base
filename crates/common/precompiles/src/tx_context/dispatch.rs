//! ABI dispatch for the EIP-8130 transaction context precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use base_precompile_storage::PrecompileResult;

use crate::{
    ITransactionContext::{self, ITransactionContextCalls as C},
    macros::decode_precompile_call,
    tx_context::storage::TxContextStorage,
};

/// EIP-8130 getter output price per 32-byte word: the EVM copy-word cost
/// (`W_copy`, revm's `gas::COPY`). Unlike the sibling nonce-manager dispatcher —
/// which prices *input* calldata words at `G_SHA3WORD` (see
/// [`crate::nonce::dispatch`]) — the transaction-context getters price the
/// *returned* words per the EIP-8130 output schedule, so the two dispatchers use
/// deliberately different word costs. `gas_matches_evm_reference` pins this to
/// revm's canonical constant as a drift tripwire.
const OUTPUT_WORD_GAS: u64 = 3;

impl TxContextStorage<'_> {
    /// ABI-dispatches transaction context calldata and prices encoded output.
    ///
    /// EIP-8130 charges [`OUTPUT_WORD_GAS`] per 32 bytes returned in addition to
    /// the precompile call's base cost. The backing transient read is unmetered,
    /// so no TLOAD opcode charge is exposed to the caller.
    pub fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        // Transaction-context getters are nonpayable; reject attached ETH first.
        if !ctx.call_value().is_zero() {
            return BasePrecompileError::revert(ITransactionContext::NonPayable {})
                .into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        }
        let result = self.inner(calldata).and_then(|output| {
            let words = u64::try_from(output.len().div_ceil(32))
                .map_err(|_| BasePrecompileError::OutOfGas)?;
            let output_cost =
                words.checked_mul(OUTPUT_WORD_GAS).ok_or(BasePrecompileError::OutOfGas)?;
            ctx.deduct_gas(output_cost)?;
            Ok(output)
        });
        // These getters never produce a gas refund, so the refund arg is 0.
        result.into_precompile_result(ctx.gas_used(), ctx.state_gas_used(), 0, |output| output)
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
    use alloy_primitives::{Address, B256, Bytes, U256, address, b256};
    use alloy_sol_types::{SolCall, SolError};
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

    /// Drift tripwire: `OUTPUT_WORD_GAS` is the EVM copy-word cost (`W_copy`). If
    /// revm reprices `gas::COPY`, this fails so the output-word charge is
    /// re-decided deliberately rather than tracked silently (mirrors the
    /// `Eip8130GasSchedule` primitive tripwire).
    #[test]
    fn gas_matches_evm_reference() {
        assert_eq!(super::OUTPUT_WORD_GAS, revm::interpreter::gas::COPY);
    }

    #[test]
    fn getters_charge_only_three_gas_for_one_output_word() {
        let calls = [
            ITransactionContext::getTransactionSenderCall {}.abi_encode(),
            ITransactionContext::getTransactionPayerCall {}.abi_encode(),
            ITransactionContext::getTransactionSenderActorIdCall {}.abi_encode(),
        ];

        for calldata in calls {
            let mut storage = HashMapStorageProvider::new(1);
            let output = StorageCtx::enter(&mut storage, |ctx| {
                TxContextStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .expect("getter should succeed");

            assert_eq!(output.bytes.len(), 32);
            assert_eq!(storage.gas_deducted(), super::OUTPUT_WORD_GAS);
        }
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
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_call_value(U256::from(1u64));
        let calldata = ITransactionContext::getTransactionSenderCall {}.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("nonzero value should revert, not fail fatally");

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(ITransactionContext::NonPayable {}.abi_encode()));
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
