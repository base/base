//! ABI dispatch for the stablecoin B-20 variant: resolves the active version from the hardfork,
//! answers `currency()`, and routes everything else to the shared B-20 dispatcher.

use alloy_primitives::Bytes;
use alloy_sol_types::{SolInterface, SolValue};
use base_common_genesis::BaseUpgrade;
use base_tiger_precompile_storage::{PrecompileResult, StorageCtx};

use crate::{
    B20Logic, B20Token, B20Variant, B20Version, Calldata, IB20Stablecoin::IB20StablecoinCalls as SC,
    PolicyAccounting, StablecoinAccounting,
};

impl<S: StablecoinAccounting, A: PolicyAccounting> B20Token<S, A> {
    /// ABI-dispatches `calldata` to the stablecoin logic active at `upgrade`.
    pub fn dispatch_stablecoin(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
    ) -> PrecompileResult {
        self.dispatch_with(ctx, calldata, B20Version::stablecoin(upgrade), |token, version| {
            token.route_stablecoin(ctx, calldata, version, false)
        })
    }

    /// Decodes calldata and routes it to `version` with optional factory-init privilege.
    pub fn route_stablecoin(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: B20Version,
        privileged: bool,
    ) -> base_tiger_precompile_storage::Result<Bytes> {
        if let Some(selector) = calldata.first_chunk::<4>().copied()
            && SC::valid_selector(selector)
        {
            return Ok(match Calldata::decode(calldata, selector)? {
                SC::currency(_) => self.accounting.currency()?.abi_encode().into(),
            });
        }
        let call = version.decode_b20(calldata)?;
        // Stablecoin precision is fixed at 6 by the protocol spec; never read from storage, to
        // avoid the zero-return window during the factory bootstrap (BOP-349/PSRC-27).
        let decimals = |_: &S| {
            Ok(B20Variant::Stablecoin.decimals().expect("stablecoin has fixed 6-decimal precision"))
        };
        self.handle_b20_call(ctx, call, B20Logic::stablecoin(version), privileged, decimals)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_tiger_precompile_storage::{HashMapStorageProvider, StorageCtx};
    use rstest::rstest;

    use crate::{
        B20Token, FakePolicyAccounting, IB20, InMemoryTokenAccounting, PolicyVersion,
        B20Version, TestStablecoinToken,
    };

    const TOKEN: Address = Address::repeat_byte(0x01);

    fn make_stablecoin_token_with_decimals(decimals: u8) -> TestStablecoinToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.decimals = decimals;
        TestStablecoinToken::with_storage_and_policy(
            accounting,
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
    }

    fn call_inner(token: &mut TestStablecoinToken, calldata: &[u8]) -> Vec<u8> {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(TOKEN);
        StorageCtx::enter(&mut storage, |ctx| {
            token.route_stablecoin(ctx, calldata, B20Version::V1, false)
        })
        .unwrap()
        .to_vec()
    }

    fn make_token() -> B20Token<InMemoryTokenAccounting, FakePolicyAccounting> {
        B20Token::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
    }

    // Uninitialized storage reads 0 during factory bootstrap; dispatch must still return 6.
    #[rstest]
    #[case(0)]
    #[case(18)]
    fn decimals_returns_fixed_six_regardless_of_storage(#[case] stored: u8) {
        let mut token = make_stablecoin_token_with_decimals(stored);
        let calldata = IB20::decimalsCall {}.abi_encode();
        let result = call_inner(&mut token, &calldata);
        assert_eq!(U256::abi_decode(&result).unwrap(), U256::from(6u8));
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut token = make_token();
        let calldata = IB20::balanceOfCall { account: Address::ZERO }.abi_encode();
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_call_value(U256::from(1u64));

        let out = StorageCtx::enter(&mut storage, |ctx| {
            token.dispatch_stablecoin(ctx, &calldata, BaseUpgrade::Beryl)
        })
        .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(out.bytes, Bytes::from(IB20::NonPayable {}.abi_encode()));
    }
}
