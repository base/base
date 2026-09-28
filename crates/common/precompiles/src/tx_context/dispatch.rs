//! ABI dispatch for the EIP-8130 transaction context precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, PrecompileResult, StorageCtx};

use crate::{
    ITransactionContext::{self, ITransactionContextCalls as C},
    macros::decode_precompile_call,
    tx_context::storage::TxContextStorage,
};

/// EIP-8130 getter output price per 32-byte word: the EVM copy-word cost
/// (`W_copy`, revm's `gas::COPY`). Unlike the sibling nonce-manager dispatcher —
/// which prices *input* calldata words at `G_SHA3WORD` (see
/// [`crate::nonce::dispatch`]) — the transaction-context getter prices the
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
            return ctx
                .error_result(BasePrecompileError::revert(ITransactionContext::NonPayable {}));
        }
        let result = self.inner(calldata).and_then(|output| {
            let words = u64::try_from(output.len().div_ceil(32))
                .map_err(|_| BasePrecompileError::OutOfGas)?;
            let output_cost =
                words.checked_mul(OUTPUT_WORD_GAS).ok_or(BasePrecompileError::OutOfGas)?;
            ctx.deduct_gas(output_cost)?;
            Ok(output)
        });
        ctx.result_output(result, |output| output)
    }

    fn inner(&self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, ITransactionContext::ITransactionContextCalls) {
            C::getTransactionPayer(_) => {
                Ok(ITransactionContext::getTransactionPayerCall::abi_encode_returns(&self.payer()?)
                    .into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address, keccak256};
    use alloy_sol_types::{SolCall, SolError};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{ITransactionContext, TxContextStorage};

    const PAYER: Address = address!("0x2222222222222222222222222222222222222222");
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
    fn dispatch_returns_resolved_payer() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).set_payer(PAYER).unwrap();
        });

        let payer =
            dispatch(&mut storage, &ITransactionContext::getTransactionPayerCall {}.abi_encode());
        assert_eq!(
            ITransactionContext::getTransactionPayerCall::abi_decode_returns(&payer).unwrap(),
            PAYER
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
    fn payer_getter_charges_only_three_gas_for_one_output_word() {
        let calldata = ITransactionContext::getTransactionPayerCall {}.abi_encode();
        let mut storage = HashMapStorageProvider::new(1);
        let output = StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("getter should succeed");

        assert_eq!(output.bytes.len(), 32);
        assert_eq!(storage.gas_deducted(), super::OUTPUT_WORD_GAS);
    }

    #[test]
    fn dispatch_falls_back_to_origin_when_unset() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

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
        let calldata = ITransactionContext::getTransactionPayerCall {}.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("nonzero value should revert, not fail fatally");

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(ITransactionContext::NonPayable {}.abi_encode()));
    }

    /// The sender and sender-actor-id getters were removed; their selectors now
    /// revert like any unknown selector.
    #[test]
    fn removed_getters_revert() {
        for signature in ["getTransactionSender()", "getTransactionSenderActorId()"] {
            let selector = &keccak256(signature)[..4];
            let mut storage = HashMapStorageProvider::new(1);
            let output = StorageCtx::enter(&mut storage, |ctx| {
                TxContextStorage::new(ctx).dispatch(ctx, selector)
            })
            .expect("removed selector should revert, not fail fatally");

            assert!(output.is_revert(), "{signature} must revert");
        }
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
