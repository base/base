#![doc = include_str!("../README.md")]

// RISC Zero guest program for TDX attestation verification.
//
// The host supplies an ABI-encoded `TdxAttestationProverInput`. The guest
// verifies the quote and collateral, then commits the ABI-encoded
// `TDXVerifierJournal` expected by the onchain TDX verifier.

#[path = "../../../atomic_shims.rs"]
mod atomic_shims;

use std::io::Read;

use alloy_sol_types::SolValue;
use base_proof_tee_tdx_attestation_prover::TdxAttestationProverInput;
use base_proof_tee_tdx_verifier::TdxVerifier;
use risc0_zkvm::guest::env;

fn main() {
    let mut input_bytes = Vec::new();
    env::stdin().read_to_end(&mut input_bytes).expect("failed to read guest stdin");

    let input =
        TdxAttestationProverInput::decode(&input_bytes).expect("failed to decode TDX input");
    let journal =
        TdxVerifier::verify(input.verifier_input()).expect("TDX attestation verification failed");

    env::commit_slice(&SolValue::abi_encode(&journal));
}
