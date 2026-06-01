//! ABI dispatch for the EIP-8130 transaction context precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    ITxContext::{self, ITxContextCalls as C},
    macros::{decode_precompile_call, deduct_calldata_cost},
    tx_context::storage::TxContextStorage,
};

impl TxContextStorage<'_> {
    /// ABI-dispatches transaction context calldata.
    pub fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        self.inner(calldata).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            |output| output,
        )
    }

    fn inner(&self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, ITxContext::ITxContextCalls) {
            C::getSender(_) => {
                Ok(ITxContext::getSenderCall::abi_encode_returns(&self.sender()?).into())
            }
            C::getPayer(_) => {
                Ok(ITxContext::getPayerCall::abi_encode_returns(&self.payer()?).into())
            }
            C::getSenderOwnerId(_) => {
                Ok(ITxContext::getSenderOwnerIdCall::abi_encode_returns(&self.sender_owner_id()?)
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

    use crate::{ITxContext, TxContextStorage};

    const SENDER: Address = address!("0x1111111111111111111111111111111111111111");
    const PAYER: Address = address!("0x2222222222222222222222222222222222222222");
    const SENDER_OWNER_ID: B256 =
        b256!("0x3333333333333333333333333333333333333333333333333333333333333333");

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
            TxContextStorage::new(ctx).set_context(SENDER, PAYER, SENDER_OWNER_ID).unwrap();
        });

        let sender = dispatch(&mut storage, &ITxContext::getSenderCall {}.abi_encode());
        assert_eq!(ITxContext::getSenderCall::abi_decode_returns(&sender).unwrap(), SENDER);

        let payer = dispatch(&mut storage, &ITxContext::getPayerCall {}.abi_encode());
        assert_eq!(ITxContext::getPayerCall::abi_decode_returns(&payer).unwrap(), PAYER);

        let owner_id = dispatch(&mut storage, &ITxContext::getSenderOwnerIdCall {}.abi_encode());
        assert_eq!(
            ITxContext::getSenderOwnerIdCall::abi_decode_returns(&owner_id).unwrap(),
            SENDER_OWNER_ID
        );
    }

    #[test]
    fn dispatch_returns_zero_when_unset() {
        let mut storage = HashMapStorageProvider::new(1);

        let sender = dispatch(&mut storage, &ITxContext::getSenderCall {}.abi_encode());
        assert_eq!(ITxContext::getSenderCall::abi_decode_returns(&sender).unwrap(), Address::ZERO);
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
