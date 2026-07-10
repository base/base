# TDX Attestation Prover Guest

RISC Zero guest program for Intel TDX attestation verification.

Build manually with the RISC Zero toolchain:

```text
rzup install
cargo +risc0 build --release --target riscv32im-risc0-zkvm-elf --ignore-rust-version
```
