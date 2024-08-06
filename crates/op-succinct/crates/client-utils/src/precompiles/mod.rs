//! Contains the [PrecompileOverride] trait implementation for the FPVM-accelerated precompiles.

use alloc::sync::Arc;
use kona_executor::PrecompileOverride;
use kona_mpt::{TrieDB, TrieDBFetcher, TrieDBHinter};
use revm::{
    handler::register::EvmHandler,
    precompile::{
        bn128, hash, identity, modexp, secp256k1, Precompile, PrecompileOutput, PrecompileResult,
        PrecompileSpecId, PrecompileWithAddress,
    },
    primitives::Bytes,
    ContextPrecompiles, State,
};

pub const PRECOMPILE_HOOK_FD: u32 = 115;

/// Create an annotated precompile that simply tracks the cycle count of a precompile.
macro_rules! create_annotated_precompile {
    ($precompile:expr, $name:expr) => {
        PrecompileWithAddress(
            $precompile.0,
            Precompile::Standard(|input: &Bytes, gas_limit: u64| -> PrecompileResult {
                let precompile = $precompile.precompile();
                match precompile {
                    Precompile::Standard(precompile) => {
                        println!(concat!("cycle-tracker-start: precompile-", $name));
                        let result = precompile(input, gas_limit);
                        println!(concat!("cycle-tracker-end: precompile-", $name));
                        result
                    }
                    _ => panic!("Annotated precompile must be a standard precompile."),
                }
            }),
        )
    };
}

/// Create an hooked precompile that executes the precompile with a "runtime" hook.
/// This is *not* safe and should only be used for stubbing out execution of slow precompiles before
/// they are fully implemented in zkVM.
macro_rules! create_hook_precompile {
    ($precompile:expr, $name:expr) => {
        PrecompileWithAddress(
            $precompile.0,
            Precompile::Standard(|input: &Bytes, gas_limit: u64| -> PrecompileResult {
                println!(concat!("cycle-tracker-start: hook-precompile-", $name));

                // Pass (address, input, gas_limit) to the hook.
                let address = $precompile.0;
                let mut input_vec = vec![];
                input_vec.extend_from_slice(&address.0.to_vec());
                input_vec.extend_from_slice(&gas_limit.to_le_bytes());
                input_vec.extend_from_slice(input.as_ref());
                sp1_zkvm::io::write(PRECOMPILE_HOOK_FD, &input_vec);

                // Read the result from the hook.
                // Note: We're manually deserializing the PrecompileResult type as it does not have
                // `serde` support.
                let result_vec = sp1_zkvm::io::read_vec();
                let result = match result_vec[0] {
                    0 => {
                        let gas_used = u64::from_le_bytes(result_vec[1..9].try_into().unwrap());
                        let bytes = Bytes::from(result_vec[9..].to_vec());
                        Ok(PrecompileOutput { gas_used, bytes })
                    }
                    1 => {
                        let is_error = result_vec[1] == 0;
                        if is_error {
                            Err(revm::precompile::PrecompileErrors::Error(
                                revm::precompile::PrecompileError::OutOfGas,
                            ))
                        } else {
                            Err(revm::precompile::PrecompileErrors::Fatal {
                                msg: "Precompile fatal error".to_string(),
                            })
                        }
                    }
                    _ => panic!("Invalid result from precompile hook."),
                };

                println!(concat!("cycle-tracker-end: hook-precompile-", $name));
                result
            }),
        )
    };
}

pub(crate) const ANNOTATED_SHA256: PrecompileWithAddress =
    create_annotated_precompile!(hash::SHA256, "sha256");
pub(crate) const ANNOTATED_RIPEMD160: PrecompileWithAddress =
    create_annotated_precompile!(hash::RIPEMD160, "ripemd160");
pub(crate) const ANNOTATED_IDENTITY: PrecompileWithAddress =
    create_annotated_precompile!(identity::FUN, "identity");
pub(crate) const ANNOTATED_BN_ADD: PrecompileWithAddress =
    create_hook_precompile!(bn128::add::ISTANBUL, "bn-add");
pub(crate) const ANNOTATED_BN_MUL: PrecompileWithAddress =
    create_hook_precompile!(bn128::mul::ISTANBUL, "bn-mul");
pub(crate) const ANNOTATED_BN_PAIR: PrecompileWithAddress =
    create_hook_precompile!(bn128::pair::ISTANBUL, "bn-pair");
pub(crate) const ANNOTATED_MODEXP: PrecompileWithAddress =
    create_annotated_precompile!(modexp::BERLIN, "modexp");
pub(crate) const ANNOTATED_ECDSA_RECOVER: PrecompileWithAddress =
    create_annotated_precompile!(secp256k1::ECRECOVER, "ecrecover");

// TODO: When we upgrade to the latest version of revm that supports kzg-rs that compiles within SP1, we can uncomment this.
// pub(crate) const ANNOTATED_KZG_POINT_EVAL: PrecompileWithAddress = create_annotated_precompile!(
//     kzg_point_evaluation::POINT_EVALUATION,
//     "kzg-point-evaluation"
// );

/// The [PrecompileOverride] implementation for the FPVM-accelerated precompiles.
#[derive(Debug)]
pub struct ZKVMPrecompileOverride<F, H>
where
    F: TrieDBFetcher,
    H: TrieDBHinter,
{
    _phantom: core::marker::PhantomData<(F, H)>,
}

impl<F, H> Default for ZKVMPrecompileOverride<F, H>
where
    F: TrieDBFetcher,
    H: TrieDBHinter,
{
    fn default() -> Self {
        Self {
            _phantom: core::marker::PhantomData::<(F, H)>,
        }
    }
}

impl<F, H> PrecompileOverride<F, H> for ZKVMPrecompileOverride<F, H>
where
    F: TrieDBFetcher,
    H: TrieDBHinter,
{
    fn set_precompiles(handler: &mut EvmHandler<'_, (), &mut State<&mut TrieDB<F, H>>>) {
        let spec_id = handler.cfg.spec_id;

        handler.pre_execution.load_precompiles = Arc::new(move || {
            let mut ctx_precompiles =
                ContextPrecompiles::new(PrecompileSpecId::from_spec_id(spec_id)).clone();

            // Extend with ZKVM-accelerated precompiles and annotated precompiles that track the cycle count.
            let override_precompiles = [
                ANNOTATED_ECDSA_RECOVER,
                ANNOTATED_SHA256,
                ANNOTATED_RIPEMD160,
                ANNOTATED_IDENTITY,
                ANNOTATED_BN_ADD,
                ANNOTATED_BN_MUL,
                ANNOTATED_BN_PAIR,
                ANNOTATED_MODEXP,
                // ANNOTATED_KZG_POINT_EVAL,
            ];
            ctx_precompiles.extend(override_precompiles);

            ctx_precompiles
        });
    }
}
