//! Contains the [PrecompileOverride] trait implementation for the FPVM-accelerated precompiles.
use alloc::sync::Arc;
use kona_mpt::{TrieDB, TrieHinter, TrieProvider};
use revm::{
    db::states::state::State,
    handler::register::EvmHandler,
    precompile::{
        bn128, secp256r1, Precompile, PrecompileResult, PrecompileSpecId, PrecompileWithAddress,
    },
    primitives::Bytes,
    ContextPrecompiles,
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
                        println!(concat!("cycle-tracker-report-start: precompile-", $name));
                        let result = precompile(input, gas_limit);
                        println!(concat!("cycle-tracker-report-end: precompile-", $name));
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

// Source: https://github.com/anton-rs/kona/blob/main/bin/client/src/fault/handler/mod.rs#L20-L42
pub fn zkvm_handle_register<F, H>(handler: &mut EvmHandler<'_, (), &mut State<&mut TrieDB<F, H>>>)
where
    F: TrieProvider,
    H: TrieHinter,
{
    let spec_id = handler.cfg.spec_id;

    handler.pre_execution.load_precompiles = Arc::new(move || {
        let mut ctx_precompiles =
            ContextPrecompiles::new(PrecompileSpecId::from_spec_id(spec_id)).clone();

        // With Fjord, EIP-7212 is activated, so we need to load the precompiles for secp256r1.
        // Alphanet does the same here: https://github.com/paradigmxyz/alphanet/blob/f28e4220a1a637c19ef6b4928e9a427560d46fcb/crates/node/src/evm.rs#L53-L56
        ctx_precompiles.extend(secp256r1::precompiles());

        // Extend with ZKVM-accelerated precompiles and annotated precompiles that track the
        // cycle count.
        let override_precompiles = [
            ANNOTATED_BN_ADD,
            ANNOTATED_BN_MUL,
            ANNOTATED_BN_PAIR,
            ANNOTATED_KZG_EVAL,
            ANNOTATED_EC_RECOVER,
        ];
        ctx_precompiles.extend(override_precompiles);

        ctx_precompiles
    });
}
