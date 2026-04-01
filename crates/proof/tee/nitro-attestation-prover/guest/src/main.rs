// RISC Zero guest program for Nitro attestation verification.
//
// NOT compiled by the host crate — exists as source for manual compilation
// with the risc0 toolchain:
//
//   rzup install
//   cargo +risc0 build --release --target riscv32im-risc0-zkvm-elf --ignore-rust-version
//
// The resulting ELF is loaded at runtime by DirectProver / BoundlessProver.

mod atomic_shims;

use std::io::Read;

use base_proof_tee_nitro_verifier::{AttestationVerifier, VerifierInput};
use risc0_zkvm::guest::env;

fn main() {
    let mut input_bytes = Vec::new();
    env::stdin().read_to_end(&mut input_bytes).expect("failed to read guest stdin");

    let input = VerifierInput::decode(&input_bytes).expect("failed to decode VerifierInput");
    let journal = AttestationVerifier::verify(&input).expect("attestation verification failed");

    env::commit_slice(&journal.encode());
}
