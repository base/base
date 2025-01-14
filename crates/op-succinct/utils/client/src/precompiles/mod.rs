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
                        if cfg!(target_os = "zkvm") {
                            println!(concat!("cycle-tracker-report-start: precompile-", $name));
                        }
                        let result = precompile(input, gas_limit);
                        if cfg!(target_os = "zkvm") {
                            println!(concat!("cycle-tracker-report-end: precompile-", $name));
                        }
                        result
                    }
                    _ => panic!("Annotated precompile must be a standard precompile."),
                }
            }),
        )
    };
}

pub(crate) const ANNOTATED_BN_ADD: PrecompileWithAddress =
    create_annotated_precompile!(bn128::add::ISTANBUL, "bn-add");
pub(crate) const ANNOTATED_BN_MUL: PrecompileWithAddress =
    create_annotated_precompile!(bn128::mul::ISTANBUL, "bn-mul");
pub(crate) const ANNOTATED_BN_PAIR: PrecompileWithAddress =
    create_annotated_precompile!(bn128::pair::ISTANBUL, "bn-pair");
pub(crate) const ANNOTATED_KZG_EVAL: PrecompileWithAddress = create_annotated_precompile!(
    revm::precompile::kzg_point_evaluation::POINT_EVALUATION,
    "kzg-eval"
);
pub(crate) const ANNOTATED_EC_RECOVER: PrecompileWithAddress =
    create_annotated_precompile!(revm::precompile::secp256k1::ECRECOVER, "ec-recover");
pub(crate) const ANNOTATED_P256_VERIFY: PrecompileWithAddress =
    create_annotated_precompile!(revm::precompile::secp256r1::P256VERIFY, "p256-verify");

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

        // Extend with ZKVM-accelerated precompiles and annotated precompiles that track the
        // cycle count.
        let override_precompiles = [
            ANNOTATED_BN_ADD,
            ANNOTATED_BN_MUL,
            ANNOTATED_BN_PAIR,
            ANNOTATED_KZG_EVAL,
            ANNOTATED_EC_RECOVER,
            ANNOTATED_P256_VERIFY,
        ];
        ctx_precompiles.extend(override_precompiles);

        ctx_precompiles
    });
}
