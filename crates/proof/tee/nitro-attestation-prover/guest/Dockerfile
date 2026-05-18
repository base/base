# syntax=docker/dockerfile:1.7
#
# Reproducible build environment for the Nitro attestation prover guest ELF.
#
# Why this exists
# ---------------
# The Justfile in this directory already pins everything that controls the
# resulting ELF bytes:
#   - exact risc0-zkvm version (Cargo.toml + Cargo.lock)
#   - codegen-units = 1
#   - --remap-path-prefix for the repo root and CARGO_HOME
#
# The one thing the Justfile cannot pin is the *toolchain itself* and the
# surrounding shell environment. This image provides a constant toolchain
# at constant filesystem paths so the same source tree always produces the
# same `shasum -a 256 target/.../base-proof-tee-nitro-verifier-guest`,
# regardless of host OS (macOS / Linux / Windows) or host CPU architecture.
# It also ensures the repo is always mounted at /build/base, fixing the
# absolute-path component of Cargo's -C metadata hash.
#
# Build the image (once)
# ----------------------
#   docker build --platform=linux/amd64 \
#       -t nitro-guest-builder \
#       crates/proof/tee/nitro-attestation-prover/guest
#
# NOTE: --platform=linux/amd64 is required because the risc0 toolchain
# (rzup) only publishes Linux binaries for x86_64. On Apple Silicon this
# runs under QEMU emulation and may take 30-60 minutes; the image only
# needs to be built once.
#
# Run a build (every time you want a fresh ELF)
# ---------------------------------------------
# Replace <base-repo> with the absolute path to the `base` repo on your host
# (the directory that contains the top-level Cargo.toml and the .git folder).
#
#   docker run --rm --platform=linux/amd64 \
#       -v <base-repo>:/build/base \
#       nitro-guest-builder
#
# After it finishes, hash the ELF on the host:
#
#   shasum -a 256 <base-repo>/crates/proof/tee/nitro-attestation-prover/guest/target/riscv32im-risc0-zkvm-elf/release/base-proof-tee-nitro-verifier-guest
#
# Optional: persist the cargo cache across runs (only affects build speed,
# not the ELF) by adding `-v nitro-guest-builder-cargo:/opt/cargo`.

FROM --platform=linux/amd64 debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# Minimal OS surface. We deliberately avoid `build-essential` (which pulls
# ~80 packages including dpkg-dev, perl-modules-5.38, etc.); the only native
# code rustc needs to link is proc-macro dylibs and the host-side
# `compute-image-id` binary, for which `gcc` + `libc6-dev` is enough.
#
#   - git: the Justfile shells out to `git rev-parse --show-toplevel`.
#   - curl, ca-certificates, xz-utils: fetch rustup / rzup / just installers.
#   - gcc, libc6-dev, pkg-config, libssl-dev: link proc-macro dylibs and
#     compile the host-side `compute-image-id` helper.
#   - mold: the workspace .cargo/config.toml at <repo>/.cargo/config.toml
#     may force `-C link-arg=-fuse-ld=mold` for the host target, which
#     cargo discovers when walking up from any subdir. Required only by the
#     host-side compute-image-id binary; the guest ELF targets
#     riscv32im-risc0-zkvm-elf and uses rust-lld, so it is unaffected by the
#     choice of host linker (i.e. installing mold here does not change the
#     guest ELF bytes).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gcc \
        git \
        libc6-dev \
        libssl-dev \
        mold \
        pkg-config \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Allow git to operate on bind-mounted directories whose owner uid does not
# match the container user. Without this, `git rev-parse --show-toplevel`
# fails with "dubious ownership" inside the Justfile.
RUN git config --system --add safe.directory /build/base

# Pinned tool versions. Bumping any of these may change the resulting ELF.
# RISC0_RUST_COMMIT is asserted at image build time so a silently-bumped
# upstream re-tag fails loudly here instead of producing a different ELF
# later.
ARG JUST_VERSION=1.36.0
ARG RISC0_RUST_VERSION=1.91.1
# Must match `expected_risc0_commit` in the Justfile.
ARG RISC0_RUST_COMMIT=30e2186d0
# SHA-256 of rust-toolchain-x86_64-unknown-linux-gnu.tar.gz from the
# r0.<RISC0_RUST_VERSION> GitHub release. Update when bumping the toolchain.
ARG RISC0_TOOLCHAIN_SHA256=e082a1dc44abdef1d95460295a70218eb294ab999b834570ec932d05641cce5d

# `just` (drives the build via the existing Justfile recipes).
RUN curl -fsSL "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
        | tar -xz -C /usr/local/bin just \
    && just --version

# Place rustup / cargo at fixed locations so the strings the Justfile feeds
# into --remap-path-prefix are constant on every host.
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/root/.risc0/bin:${PATH}

# Install rustup with NO default toolchain — we only need rustup's machinery
# so that rzup can register the risc0 toolchain. The risc0 rust 1.91.1
# toolchain then doubles as both the host compiler (for compute-image-id)
# and the RISC-V cross compiler (for the guest ELF), so we don't need a
# separate stable host rust.
RUN curl -fsSL https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain none --profile minimal \
    && rustup --version

# Install the pinned risc0 rust toolchain. We skip rzup entirely and do a
# manual install (extract + symlink) for two reasons:
#   1. rzup's install script rejects environments without an existing rustc
#   2. Running rzup under QEMU (Apple Silicon building amd64) is extremely
#      slow — the ~500MB download alone takes 10+ minutes
#
# Speed optimization: if rust-toolchain-x86_64-unknown-linux-gnu.tar.gz is
# present in the Docker build context, we COPY it to skip the download.
# Pre-download on the host with:
#   gh release download r0.1.91.1 --repo risc0/rust \
#       --pattern "rust-toolchain-x86_64-unknown-linux-gnu.tar.gz" \
#       --dir crates/proof/tee/nitro-attestation-prover/guest
COPY rust-toolchain-x86_64-unknown-linux-gnu.tar.gz* /tmp/
RUN TOOLCHAIN_TAR="/tmp/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz" \
    && TOOLCHAIN_DIR="/root/.risc0/toolchains/v${RISC0_RUST_VERSION}-rust-x86_64-unknown-linux-gnu" \
    && mkdir -p "$TOOLCHAIN_DIR" /root/.risc0/bin \
    && if [ -f "$TOOLCHAIN_TAR" ]; then \
        echo "Using pre-downloaded toolchain from build context"; \
    else \
        echo "Downloading toolchain from GitHub (this may be slow under QEMU)..."; \
        curl -fsSL -o "$TOOLCHAIN_TAR" \
            "https://github.com/risc0/rust/releases/download/r0.${RISC0_RUST_VERSION}/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz"; \
    fi \
    && echo "${RISC0_TOOLCHAIN_SHA256}  ${TOOLCHAIN_TAR}" | sha256sum -c - \
    && tar -xzf "$TOOLCHAIN_TAR" -C "$TOOLCHAIN_DIR" \
    && rm "$TOOLCHAIN_TAR" \
    && rustup toolchain link risc0 "$TOOLCHAIN_DIR" \
    && rustup default risc0 \
    && rustc --version > /tmp/rustc.txt \
    && cat /tmp/rustc.txt \
    && grep -q "${RISC0_RUST_COMMIT}" /tmp/rustc.txt

# The host's `base/` repo is expected to be bind-mounted at /build/base, so
# this resolves to the guest crate inside the bind mount.
WORKDIR /build/base/crates/proof/tee/nitro-attestation-prover/guest

# Default to `just bundle`. Override with e.g.
#   docker run ... nitro-guest-builder build
#   docker run ... nitro-guest-builder diagnose
ENTRYPOINT ["just"]
CMD ["bundle"]
