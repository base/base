# Nitro Verifier Guest Program

RISC Zero guest program that verifies AWS Nitro Enclave attestation documents
inside the zkVM.

This directory is a **standalone Cargo workspace** (note the `[workspace]` in
`Cargo.toml`) and is intentionally **not** a member of the repository workspace.
The guest targets `riscv32im-risc0-zkvm-elf` and requires the risc0 toolchain,
so including it in the main workspace would break normal `cargo build` / `cargo
check` invocations for everyone who doesn't have that toolchain installed.

## Quick start

Build the Docker image (once), then build the ELF and bundle it:

```sh
# From the repository root:

# 1. Build the builder image (once)
docker build --platform=linux/amd64 \
    -t nitro-guest-builder \
    crates/proof/tee/nitro-attestation-prover/guest

# 2. Build ELF, bundle into R0BF, and compute image ID
docker run --rm --platform=linux/amd64 \
    -v "$(pwd)":/build/base \
    nitro-guest-builder
```

Step 2 prints the **image ID** and writes two artifacts:

| Artifact | Path | Description |
|---|---|---|
| Raw ELF | `target/riscv32im-risc0-zkvm-elf/release/base-proof-tee-nitro-verifier-guest` | The compiled guest binary |
| R0BF bundle | `target/base-proof-tee-nitro-verifier-guest.r0bf` | ELF + risc0 v1compat kernel (uploaded to IPFS) |

### Verifying your build

Compare your local build against another builder and against the production
IPFS upload. There are three distinct hashes — make sure you compare like
with like:

```sh
# 3a. Hash the raw ELF (compare with other builders' ELF hash)
shasum -a 256 crates/proof/tee/nitro-attestation-prover/guest/target/riscv32im-risc0-zkvm-elf/release/base-proof-tee-nitro-verifier-guest

# 3b. Hash the R0BF bundle (compare with the production IPFS upload)
shasum -a 256 crates/proof/tee/nitro-attestation-prover/guest/target/base-proof-tee-nitro-verifier-guest.r0bf

# 3c. Download the R0BF from IPFS and verify it matches your local bundle.
#     Obtain the IPFS gateway URL from the config service
#     (the --boundless-verifier-program-url value).
curl -fsSL -o /tmp/guest-remote.r0bf "<IPFS_GATEWAY_URL>"
shasum -a 256 /tmp/guest-remote.r0bf
# This should match the hash from step 3b.
```

The **image ID** printed by step 2 is a third value — it is risc0's own hash
of the ELF (not a SHA-256). This is the value used on-chain and in the
registrar `--image-id` config.

### Apple Silicon note

The risc0 toolchain only publishes x86_64 Linux binaries, so the Docker
image build runs under QEMU emulation and can be slow. To speed it up,
pre-download the ~500MB toolchain tarball on the host before building the
image:

```sh
gh release download r0.1.91.1 --repo risc0/rust \
    --pattern "rust-toolchain-x86_64-unknown-linux-gnu.tar.gz" \
    --dir crates/proof/tee/nitro-attestation-prover/guest
```

The Dockerfile detects and uses the pre-downloaded file automatically.
The tarball is git-ignored and can be deleted after the image is built.

## Full workflow

### 1. Build and bundle

```sh
docker run --rm --platform=linux/amd64 \
    -v /path/to/base-repo:/build/base \
    nitro-guest-builder
```

This runs two steps inside the container:
- **Build**: compiles the guest ELF with `cargo +risc0` for `riscv32im-risc0-zkvm-elf`
- **Bundle**: combines the raw ELF with the risc0 v1compat kernel into R0BF
  (RISC Zero Binary Format) and computes the image ID

### 2. Upload to IPFS

Upload the bundled R0BF file (`target/base-proof-tee-nitro-verifier-guest.r0bf`)
to IPFS (e.g. via Pinata). Note the resulting gateway URL.

### 3. Update configuration

Three values must all match the same build:

| Where | Value |
|---|---|
| Registrar CLI `--image-id` | Image ID printed by the build |
| Registrar CLI `--boundless-verifier-program-url` | IPFS gateway URL from step 2 |
| On-chain `TEEProverRegistry` contract | Same image ID, set via admin transaction |

## Other Docker commands

```sh
# Build only (raw ELF, no bundling)
docker run --rm --platform=linux/amd64 \
    -v /path/to/base-repo:/build/base \
    nitro-guest-builder build

# Dump build environment for debugging reproducibility issues
docker run --rm --platform=linux/amd64 \
    -v /path/to/base-repo:/build/base \
    nitro-guest-builder diagnose

# Persist the cargo cache across runs (only affects build speed, not the ELF)
# IMPORTANT: after rebuilding the image (e.g. toolchain bump), delete the
# old volume to pick up the new binaries:
#   docker volume rm nitro-guest-builder-cargo
docker run --rm --platform=linux/amd64 \
    -v /path/to/base-repo:/build/base \
    -v nitro-guest-builder-cargo:/opt/cargo \
    nitro-guest-builder
```

## Reproducibility

All builds **must** use Docker. The Dockerfile pins the OS, toolchain binary,
filesystem paths, and compiler flags to produce byte-identical ELFs regardless
of the host machine. Do not build natively — different host compilers (e.g.
macOS ARM vs Linux x86_64) produce different cross-compiled RISC-V output.

The image ID is a hash of the ELF binary. For the same source code to always
produce the same image ID, the ELF must be byte-identical across builds.
The Docker environment ensures this through:

### Fixed toolchain binary

The Dockerfile installs the exact risc0 rust toolchain (pinned version +
commit hash assertion) at a fixed filesystem path. Every builder gets the
same `rustc` binary.

### Single codegen unit

Rust's default release profile uses 16 parallel codegen units. Parallel
codegen can produce different output depending on thread scheduling, making
the ELF non-deterministic across machines. `Cargo.toml` sets
`codegen-units = 1` in the release profile to force single-threaded codegen.

### Dependency pinning

The `risc0-zkvm` dependency is pinned to an exact version (`=x.y.z`) in
`Cargo.toml` and the `Cargo.lock` is committed, so dependency resolution is
deterministic.

### Path remapping

Rust embeds absolute file paths into the binary for panic messages. The
Justfile passes `--remap-path-prefix` flags via `RUSTFLAGS` to normalize
these paths to `/build` and `/registry`.

### Fixed build path

Cargo includes the absolute filesystem path of path dependencies in its
`-C metadata` hash, which feeds into symbol mangling. Docker ensures the
repo is always bind-mounted at `/build/base`, so this path is constant
across machines. This is why native builds are not supported — different
checkout paths would produce different ELFs.

### Diagnosing reproducibility issues

If two machines produce different ELF hashes, run `diagnose` on both
and compare the output:

```sh
docker run --rm --platform=linux/amd64 \
    -v /path/to/base-repo:/build/base \
    nitro-guest-builder diagnose
```

### Bumping versions

When bumping risc0 versions, you **must** rebuild the ELF, re-upload to
IPFS, and update the image ID in both the registrar config and the on-chain
contract. Otherwise the image IDs will diverge and proof verification will
fail.
