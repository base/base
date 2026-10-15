#!/bin/sh
set -eu

component=${1:?component required}
case "$component" in
    enclave)
        apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig perl make \
            clang clang-dev git mold protobuf-dev wget zlib-dev zlib-static
        exit 0 ;;
    linuxkit)
        version=${LINUXKIT_VERSION:-v1.8.2}
        sha256=${LINUXKIT_SHA256:-18b97b0d83401dd88ade14f207eb26358d9a7fde5d3dc4b1e24bd8faf087be6f}
        wget -q -O /usr/local/bin/linuxkit \
            "https://github.com/linuxkit/linuxkit/releases/download/${version}/linuxkit-linux-amd64"
        echo "${sha256}  /usr/local/bin/linuxkit" | sha256sum -c -
        chmod +x /usr/local/bin/linuxkit
        exit 0 ;;
    eif-tool) packages="ca-certificates git build-essential pkg-config libssl-dev" ;;
    nitro-cli) packages="ca-certificates git build-essential curl pkg-config libssl-dev" ;;
    proposer) packages="git libclang-dev pkg-config curl build-essential mold protobuf-compiler" ;;
    challenger|challenger-e2e) packages="git libclang-dev pkg-config curl build-essential mold protobuf-compiler libprotobuf-dev" ;;
    registrar|nitro-host) packages="git libclang-dev pkg-config curl build-essential mold protobuf-compiler libssl-dev" ;;
    prover-service) packages="git libclang-dev libssl-dev pkg-config curl build-essential mold protobuf-compiler" ;;
    zk-host|snark-e2e) packages="git libclang-dev libssl-dev pkg-config curl build-essential cmake zlib1g-dev mold unzip wget" ;;
    *) echo "unknown install component: $component" >&2; exit 1 ;;
esac
apt-get update
# Package names above are fixed, whitespace-separated words.
# shellcheck disable=SC2086
apt-get install -y --no-install-recommends $packages
rm -rf /var/lib/apt/lists/*

case "$component" in
    nitro-cli)
        rustup_version=${RUSTUP_VERSION:-1.29.1}
        rust_toolchain=${RUST_TOOLCHAIN:-1.99.0}
        case "$(uname -m)" in
            x86_64)
                rustup_target=x86_64-unknown-linux-gnu
                rustup_sha256=${RUSTUP_SHA256_AMD64:-dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71} ;;
            aarch64|arm64)
                rustup_target=aarch64-unknown-linux-gnu
                rustup_sha256=${RUSTUP_SHA256_ARM64:-15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433} ;;
            *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
        esac
        # rustup-init dispatches by executable basename.
        installer=$(mktemp "${TMPDIR:-/tmp}/rustup-init.XXXXXX")
        trap 'rm -f "$installer"' EXIT
        curl --proto '=https' --tlsv1.2 -sSf \
            "https://static.rust-lang.org/rustup/archive/${rustup_version}/${rustup_target}/rustup-init" \
            -o "$installer"
        echo "${rustup_sha256}  ${installer}" | sha256sum -c -
        chmod +x "$installer"
        "$installer" -y --default-toolchain "$rust_toolchain"
        rust_version=$("$HOME/.cargo/bin/rustc" --version)
        echo "$rust_version"
        case "$rust_version" in
            "rustc $rust_toolchain "*) ;;
            *) echo "unexpected Rust compiler version: $rust_version" >&2; exit 1 ;;
        esac ;;
    prover-service)
        cargo install sqlx-cli --version "${SQLX_CLI_VERSION:-0.8.6}" --locked \
            --no-default-features --features postgres,native-tls ;;
    zk-host|snark-e2e)
        go_version=${GO_VERSION:-1.24.0}
        protoc_version=${PROTOC_VERSION:-33.4}
        case "$(uname -m)" in
            x86_64)
                go_arch=amd64
                go_sha256=${GO_SHA256_AMD64:-dea9ca38a0b852a74e81c26134671af7c0fbe65d81b0dc1c5bfe22cf7d4c8858}
                protoc_arch=x86_64
                protoc_sha256=${PROTOC_SHA256_X86_64:-c0040ea9aef08fdeb2c74ca609b18d5fdbfc44ea0042fcfbfb38860d35f7dd66} ;;
            aarch64|arm64)
                go_arch=arm64
                go_sha256=${GO_SHA256_ARM64:-c3fa6d16ffa261091a5617145553c71d21435ce547e44cc6dfb7470865527cc7}
                protoc_arch=aarch_64
                protoc_sha256=${PROTOC_SHA256_AARCH64:-15aa988f4a6090636525ec236a8e4b3aab41eef402751bd5bb2df6afd9b7b5a5} ;;
            *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
        esac
        go_archive="go${go_version}.linux-${go_arch}.tar.gz"
        wget -q "https://go.dev/dl/${go_archive}"
        echo "${go_sha256}  ${go_archive}" | sha256sum -c -
        tar -C /usr/local -xzf "$go_archive"
        rm "$go_archive"
        protoc_zip="protoc-${protoc_version}-linux-${protoc_arch}.zip"
        wget -q "https://github.com/protocolbuffers/protobuf/releases/download/v${protoc_version}/${protoc_zip}"
        echo "${protoc_sha256}  ${protoc_zip}" | sha256sum -c -
        unzip "$protoc_zip" -d /usr/local
        rm "$protoc_zip" ;;
esac
