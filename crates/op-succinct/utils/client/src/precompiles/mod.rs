//! Contains the [PrecompileOverride] trait implementation for the FPVM-accelerated precompiles.
use alloc::sync::Arc;
use kona_executor::{TrieDB, TrieDBProvider};
use kona_mpt::TrieHinter;
use revm::{
    db::states::state::State,
    handler::register::EvmHandler,
    precompile::{bn128, Precompile, PrecompileResult, PrecompileWithAddress},
    primitives::{spec_to_generic, Bytes, SpecId},
};

/// Create an annotated precompile that simply tracks the cycle count of a precompile.
macro_rules! create_annotated_precompile {
    ($precompile:expr, $name:expr) => {
        PrecompileWithAddress(
            $precompile.0,
            Precompile::Standard(|input: &Bytes, gas_limit: u64| -> PrecompileResult {
                let precompile = $precompile.precompile();
                match precompile {
                    Precompile::Standard(precompile) => {
                        #[cfg(target_os = "zkvm")]
                        println!(concat!("cycle-tracker-report-start: precompile-", $name));

                        let result = precompile(input, gas_limit);

                        #[cfg(target_os = "zkvm")]
                        println!(concat!("cycle-tracker-report-end: precompile-", $name));

                        result
                    }
                    _ => panic!("Annotated precompile must be a standard precompile."),
                }
            }),
        )
    };
}

/// Tuples of the original and annotated precompiles.
// TODO: Add kzg_point_evaluation once it has standard precompile support in revm-precompile 0.17.0.
const PRECOMPILES: &[(PrecompileWithAddress, PrecompileWithAddress)] = &[
    (
        bn128::add::ISTANBUL,
        create_annotated_precompile!(bn128::add::ISTANBUL, "bn-add"),
    ),
    (
        bn128::mul::ISTANBUL,
        create_annotated_precompile!(bn128::mul::ISTANBUL, "bn-mul"),
    ),
    (
        bn128::pair::ISTANBUL,
        create_annotated_precompile!(bn128::pair::ISTANBUL, "bn-pair"),
    ),
    (
        revm::precompile::secp256k1::ECRECOVER,
        create_annotated_precompile!(revm::precompile::secp256k1::ECRECOVER, "ec-recover"),
    ),
    (
        revm::precompile::secp256r1::P256VERIFY,
        create_annotated_precompile!(revm::precompile::secp256r1::P256VERIFY, "p256-verify"),
    ),
];

// Source: https://github.com/anton-rs/kona/blob/main/bin/client/src/fault/handler/mod.rs#L20-L42
pub fn zkvm_handle_register<F, H>(handler: &mut EvmHandler<'_, (), &mut State<&mut TrieDB<F, H>>>)
where
    F: TrieDBProvider,
    H: TrieHinter,
{
    let spec_id = handler.cfg.spec_id;

    handler.pre_execution.load_precompiles = Arc::new(move || {
        let mut ctx_precompiles = spec_to_generic!(spec_id, {
            revm::optimism::load_precompiles::<SPEC, (), &mut State<&mut TrieDB<F, H>>>()
        });
        // Add the annotated precompiles.
        ctx_precompiles.extend(PRECOMPILES.iter().map(|p| p.1.clone()).take(1));
        ctx_precompiles
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_precompile_standard() {
        // Check each precompile which was annotated is a standard precompile.
        for precompile in PRECOMPILES {
            assert!(
                matches!(precompile.0 .1, Precompile::Standard(_)),
                "{:?} is not a standard precompile",
                precompile.0
            );
        }
    }
}
