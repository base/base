# Nitro Verifier Guest Program

RISC Zero guest program that verifies AWS Nitro Enclave attestation documents
inside the zkVM.

This directory is a **standalone Cargo workspace** (note the `[workspace]` in
`Cargo.toml`) and is intentionally **not** a member of the repository workspace.
The guest targets `riscv32im-risc0-zkvm-elf` and requires the risc0 toolchain,
so including it in the main workspace would break normal `cargo build` / `cargo
check` invocations for everyone who doesn't have that toolchain installed.

## Building

```sh
# Install the risc0 toolchain
rzup install

# Compile the guest ELF
cargo +risc0 build --release --target riscv32im-risc0-zkvm-elf

# The ELF is at:
# target/riscv32im-risc0-zkvm-elf/release/base-proof-tee-nitro-verifier-guest
```

The resulting ELF is loaded at runtime by `DirectProver` or `BoundlessProver`
in the host crate. It is not embedded at compile time.
