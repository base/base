# Nitro Verifier Guest Program

RISC Zero guest program that verifies AWS Nitro Enclave attestation documents
inside the zkVM.

This directory is a **standalone Cargo workspace** (note the `[workspace]` in
`Cargo.toml`) and is intentionally **not** a member of the repository workspace.
The guest targets `riscv32im-risc0-zkvm-elf` and requires the risc0 toolchain,
so including it in the main workspace would break normal `cargo build` / `cargo
check` invocations for everyone who doesn't have that toolchain installed.

## Quick start

Build the ELF, bundle it into R0BF format, and compute the image ID in one step:

```sh
just bundle
```

The output shows the **image ID** and writes the bundled R0BF file to
`target/base-proof-tee-nitro-verifier-guest.r0bf`.

## Full workflow

### 1. Install the risc0 toolchain

```sh
rzup install
# or: just install-toolchain
```

### 2. Build and bundle

```sh
just bundle
```

This runs two steps:
- **Build**: compiles the guest ELF with `cargo +risc0` for `riscv32im-risc0-zkvm-elf`
- **Bundle**: combines the raw ELF with the risc0 v1compat kernel into R0BF
  (RISC Zero Binary Format) and computes the image ID

We use a two-step approach (manual build + `compute-image-id` tool) rather than
`cargo risczero bake` because `bake` does not pass `--ignore-rust-version` to
cargo, and the `base-proof-tee-nitro-verifier` dependency inherits an MSRV from
the workspace that is newer than the risc0 toolchain's rustc.

### 3. Upload to IPFS

Upload the bundled R0BF file (`target/base-proof-tee-nitro-verifier-guest.r0bf`)
to IPFS (e.g. via Pinata). Note the resulting gateway URL.

### 4. Update configuration

Three values must all match the same build:

| Where | Value |
|---|---|
| Registrar CLI `--image-id` | Image ID printed by `just bundle` |
| Registrar CLI `--boundless-verifier-program-url` | IPFS gateway URL from step 3 |
| On-chain `TEEProverRegistry` contract | Same image ID, set via admin transaction |

## Individual commands

If you need to run steps separately:

```sh
# Build only (raw ELF)
just build

# Compute image ID from an existing ELF or R0BF file
RISC0_SKIP_BUILD_KERNELS=1 cargo run \
    --manifest-path tools/compute-image-id/Cargo.toml -- <path-to-elf-or-r0bf>

# Compute image ID and write bundled R0BF
RISC0_SKIP_BUILD_KERNELS=1 cargo run \
    --manifest-path tools/compute-image-id/Cargo.toml -- <path-to-elf> \
    --output <output-path.r0bf>
```

## Reproducibility

The image ID is a hash of the ELF binary. For the same source code to always
produce the same image ID, the ELF must be byte-identical across builds.
Two things ensure this:

### Dependency pinning

The `risc0-zkvm` dependency is pinned to an exact version (`=x.y.z`) in
`Cargo.toml` and the `Cargo.lock` is committed, so dependency resolution is
deterministic.

### Path remapping

Rust embeds absolute file paths into the binary for panic messages (e.g.
`panicked at /Users/you/project/src/foo.rs:42`). These paths change depending
on where the repository is checked out, which produces a different ELF hash
and therefore a different image ID — even from identical source code.

The Justfile passes `--remap-path-prefix` flags via `RUSTFLAGS` to normalize
these paths:

- The repository checkout path is remapped to `/build`
- The Cargo registry (`$CARGO_HOME`) is remapped to `/registry`

**Always use `just build` / `just bundle`** to get reproducible builds. Running
`cargo +risc0 build` directly will produce a working ELF but with
machine-specific paths baked in.

### Bumping versions

When bumping risc0 versions, you **must** rebuild the ELF, re-upload to
IPFS, and update the image ID in both the registrar config and the on-chain
contract. Otherwise the image IDs will diverge and proof verification will
fail.
