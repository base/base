//! ABI dispatch for the EIP-8130 2D nonce manager precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use base_precompile_storage::PrecompileResult;

use crate::{
    INonceManager::{self, INonceManagerCalls as C},
    macros::decode_precompile_call,
    nonce::storage::NonceManagerStorage,
};

/// Per-word *input* calldata gas charge (`G_SHA3WORD`, revm's
/// `gas::KECCAK256WORD`), matching common Base precompile dispatch. This is the
/// input-word model; the sibling transaction-context dispatcher instead prices
/// *output* words at `W_copy` (see [`crate::tx_context::dispatch`]), so the two
/// dispatchers deliberately use different word costs.
const CALLDATA_WORD_GAS: u64 = 6;

impl NonceManagerStorage<'_> {
    /// ABI-dispatches nonce manager calldata.
    ///
    /// Only the read-only `getNonce` getter is reachable through the ABI; the
    /// nonce-mutating entry points (`increment_nonce`, `check_and_mark_expiring_nonce`)
    /// are driven by the EIP-8130 execution layer, not by EVM calls.
    pub fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        // `getNonce` is nonpayable; reject attached ETH before charging calldata gas.
        if !ctx.call_value().is_zero() {
            return BasePrecompileError::revert(INonceManager::NonPayable {})
                .into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        }
        let calldata_cost = (calldata.len() as u64).div_ceil(32).saturating_mul(CALLDATA_WORD_GAS);
        if let Err(error) = ctx.deduct_gas(calldata_cost) {
            return error.into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        }
        // `getNonce` is a read-only getter and never produces a gas refund.
        self.inner(calldata).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            0,
            |output| output,
        )
    }

    fn inner(&self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, INonceManager::INonceManagerCalls) {
            C::getNonce(call) => Ok(INonceManager::getNonceCall::abi_encode_returns(
                &self.get_nonce(call.account, call.nonceKey)?,
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address};
    use alloy_sol_types::{SolCall, SolError};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{INonceManager, NonceManagerStorage};

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");

    fn dispatch(
        storage: &mut HashMapStorageProvider,
        calldata: &[u8],
    ) -> base_precompile_storage::PrecompileOutput {
        StorageCtx::enter(storage, |ctx| NonceManagerStorage::new(ctx).dispatch(ctx, calldata))
            .expect("dispatch should not fail fatally")
    }

    /// Drift tripwire: `CALLDATA_WORD_GAS` is `G_SHA3WORD`. If revm reprices
    /// `gas::KECCAK256WORD`, this fails so the input-word charge is re-decided
    /// deliberately rather than tracked silently.
    #[test]
    fn gas_matches_evm_reference() {
        assert_eq!(super::CALLDATA_WORD_GAS, revm::interpreter::gas::KECCAK256WORD);
    }

    #[test]
    fn dispatch_get_nonce_returns_current_value() {
        let mut storage = HashMapStorageProvider::new(1);
        let nonce_key = U256::from(9);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            mgr.increment_nonce(ACCOUNT, nonce_key).unwrap();
            mgr.increment_nonce(ACCOUNT, nonce_key).unwrap();
        });

        let calldata =
            INonceManager::getNonceCall { account: ACCOUNT, nonceKey: nonce_key }.abi_encode();
        let output = dispatch(&mut storage, &calldata);

        assert!(!output.is_revert());
        assert_eq!(INonceManager::getNonceCall::abi_decode_returns(&output.bytes).unwrap(), 2);
    }

    #[test]
    fn dispatch_get_nonce_reverts_for_protocol_nonce() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata =
            INonceManager::getNonceCall { account: ACCOUNT, nonceKey: U256::ZERO }.abi_encode();
        let output = dispatch(&mut storage, &calldata);

        assert!(output.is_revert());
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_call_value(U256::from(1u64));
        let calldata =
            INonceManager::getNonceCall { account: ACCOUNT, nonceKey: U256::from(9) }.abi_encode();

        let output = dispatch(&mut storage, &calldata);

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(INonceManager::NonPayable {}.abi_encode()));
    }

    #[test]
    fn dispatch_reverts_on_unknown_selector() {
        let mut storage = HashMapStorageProvider::new(1);
        let output = dispatch(&mut storage, &[0xde, 0xad, 0xbe, 0xef]);

        assert!(output.is_revert());
    }
}
