//! Base precompile provider integration.

use alloc::string::ToString;

use base_common_consensus::{
    INonceManager, NONCE_MANAGER_ADDRESS, TX_CONTEXT_ADDRESS, TxContextValues, handle_tx_context,
    nonce_slot,
};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput, PrecompilesMap};
use alloy_sol_types::{SolCall, SolType, sol_data};

use crate::BaseSpecId;

/// Base precompile installer for the Base EVM spec.
pub type BasePrecompileInstaller = base_common_precompiles::BasePrecompileInstaller<BaseSpecId>;

/// Base precompile provider for the Base EVM spec.
pub type BasePrecompiles = base_common_precompiles::BasePrecompiles<BaseSpecId>;

#[cfg(feature = "std")]
std::thread_local! {
    static EIP8130_TX_CONTEXT: core::cell::RefCell<Option<TxContextValues>> =
        const { core::cell::RefCell::new(None) };
}

/// Installs Base precompiles and extends the map with EIP-8130 AA precompiles.
pub fn install_base_precompiles(spec: BaseSpecId) -> PrecompilesMap {
    let mut precompiles = BasePrecompileInstaller::new(spec).install();
    extend_base_precompiles(&mut precompiles);
    precompiles
}

/// Adds EIP-8130 AA precompiles to a [`PrecompilesMap`].
pub fn extend_base_precompiles(precompiles: &mut PrecompilesMap) {
    precompiles.extend_precompiles([
        (NONCE_MANAGER_ADDRESS, nonce_manager_precompile()),
        (TX_CONTEXT_ADDRESS, tx_context_precompile()),
    ]);
}

/// Sets the transaction context exposed by the EIP-8130 TxContext precompile.
pub fn set_eip8130_tx_context(values: Option<TxContextValues>) {
    #[cfg(feature = "std")]
    EIP8130_TX_CONTEXT.with(|ctx| *ctx.borrow_mut() = values);

    #[cfg(not(feature = "std"))]
    let _ = values;
}

/// Clears the transaction context exposed by the EIP-8130 TxContext precompile.
pub fn clear_eip8130_tx_context() {
    set_eip8130_tx_context(None);
}

fn get_eip8130_tx_context() -> Option<TxContextValues> {
    #[cfg(feature = "std")]
    {
        EIP8130_TX_CONTEXT.with(|ctx| ctx.borrow().clone())
    }

    #[cfg(not(feature = "std"))]
    {
        None
    }
}

fn nonce_manager_precompile() -> DynPrecompile {
    DynPrecompile::new_stateful(PrecompileId::custom("base-eip8130-nonce-manager"), |input| {
        run_nonce_manager_precompile(input)
    })
}

fn tx_context_precompile() -> DynPrecompile {
    DynPrecompile::new_stateful(PrecompileId::custom("base-eip8130-tx-context"), |input| {
        run_tx_context_precompile(input)
    })
}

fn run_nonce_manager_precompile(mut input: PrecompileInput<'_>) -> PrecompileResult {
    let call = INonceManager::getNonceCall::abi_decode(input.data)
        .map_err(|_| PrecompileError::other("invalid NonceManager input"))?;
    let nonce_key = call.nonceKey.into();
    let slot = nonce_slot(call.account, nonce_key);
    let value = input
        .internals
        .sload(NONCE_MANAGER_ADDRESS, slot.into())
        .map_err(|err| PrecompileError::other(err.to_string()))?
        .data
        .to::<u64>();
    let output = <sol_data::Uint<64>>::abi_encode(&value).into();
    precompile_output(input.gas, base_common_consensus::NONCE_MANAGER_GAS, output)
}

fn run_tx_context_precompile(input: PrecompileInput<'_>) -> PrecompileResult {
    let values = get_eip8130_tx_context().unwrap_or_default();
    match handle_tx_context(&values, input.data) {
        Ok((gas_used, output)) => precompile_output(input.gas, gas_used, output),
        Err(error) => Ok(PrecompileOutput::new_reverted(0, error.to_string().into_bytes().into())),
    }
}

fn precompile_output(
    gas_limit: u64,
    gas_used: u64,
    output: revm::primitives::Bytes,
) -> PrecompileResult {
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }
    Ok(PrecompileOutput::new(gas_used, output))
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use revm::{
        precompile::{PrecompileError, bn254, modexp, secp256r1},
        primitives::eip7823,
    };

    use super::*;
    use crate::BaseUpgrade;

    fn encode_length(len: usize) -> [u8; 32] {
        let mut encoded = [0u8; 32];
        encoded[24..].copy_from_slice(&(len as u64).to_be_bytes());
        encoded
    }

    fn oversized_modexp_input() -> Vec<u8> {
        let mut input = Vec::with_capacity(96);
        input.extend_from_slice(&encode_length(eip7823::INPUT_SIZE_LIMIT + 1));
        input.extend_from_slice(&encode_length(0));
        input.extend_from_slice(&encode_length(1));
        input
    }

    #[test]
    fn base_spec_id_selects_jovian_precompile_limits() {
        let precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian));
        let bn254_pair = precompiles.precompiles().get(&bn254::pair::ADDRESS).unwrap();

        let input = vec![0u8; 81_984 + bn254::PAIR_ELEMENT_LEN];
        assert!(matches!(
            bn254_pair.execute(&input, u64::MAX),
            Err(PrecompileError::Bn254PairLength)
        ));
    }

    #[test]
    fn base_spec_id_selects_azul_osaka_precompile_rules() {
        let jovian_precompiles =
            BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian));
        let azul_precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul));

        let jovian_p256 =
            jovian_precompiles.precompiles().get(secp256r1::P256VERIFY.address()).unwrap();
        let azul_p256 =
            azul_precompiles.precompiles().get(secp256r1::P256VERIFY_OSAKA.address()).unwrap();

        assert!(jovian_p256.execute(&[], 5_000).is_ok());
        assert!(matches!(azul_p256.execute(&[], 5_000), Err(PrecompileError::OutOfGas)));

        let azul_modexp = azul_precompiles.precompiles().get(modexp::OSAKA.address()).unwrap();
        assert!(matches!(
            azul_modexp.execute(&oversized_modexp_input(), u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
    }
}
