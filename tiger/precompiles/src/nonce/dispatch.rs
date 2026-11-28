//! ABI dispatch for the Base transaction 2D nonce manager precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_tiger_precompile_storage::{BasePrecompileError, PrecompileResult, StorageCtx};

use crate::{
    Calldata,
    INonceManager::{self, INonceManagerCalls as C},
    nonce::storage::NonceManagerStorage,
};

impl NonceManagerStorage<'_> {
    /// ABI-dispatches nonce manager calldata.
    ///
    /// Only the read-only `getNonce` getter is reachable through the ABI; the
    /// nonce-mutating entry points (`increment_nonce`, `check_and_mark_expiring_nonce`)
    /// are driven by the Base transaction execution layer, not by EVM calls.
    pub fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        // `getNonce` is nonpayable; reject attached ETH before charging calldata gas.
        if !ctx.call_value().is_zero() {
            return ctx.error_result(BasePrecompileError::revert(INonceManager::NonPayable {}));
        }
        // Input calldata is priced per word, unlike the transaction context's output words.
        if let Err(error) = Calldata::deduct_gas(ctx, calldata) {
            return ctx.error_result(error);
        }
        // `getNonce` is a read-only getter and never produces a gas refund.
        ctx.result_output(self.inner(calldata), |output| output)
    }

    fn inner(&self, calldata: &[u8]) -> base_tiger_precompile_storage::Result<Bytes> {
        match Calldata::decode_call(calldata)? {
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
    use base_tiger_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{INonceManager, NonceManagerStorage};

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");

    fn dispatch(
        storage: &mut HashMapStorageProvider,
        calldata: &[u8],
    ) -> base_tiger_precompile_storage::PrecompileOutput {
        StorageCtx::enter(storage, |ctx| NonceManagerStorage::new(ctx).dispatch(ctx, calldata))
            .expect("dispatch should not fail fatally")
    }

    /// Drift tripwire: the calldata word gas equals `G_SHA3WORD`.
    #[test]
    fn gas_matches_evm_reference() {
        assert_eq!(
            crate::Calldata::WORD_GAS,
            base_tiger_precompile_storage::GasSchedule::KECCAK256_WORD
        );
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

    #[test]
    fn dispatch_returns_selector_only_for_decode_failure_at_cobalt() {
        let mut storage = HashMapStorageProvider::new_with_storage_features(
            1,
            base_tiger_precompile_storage::StorageFeatures::Cobalt,
        );
        let selector = INonceManager::getNonceCall::SELECTOR;

        let output = dispatch(&mut storage, &selector);

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(selector));
    }
}
