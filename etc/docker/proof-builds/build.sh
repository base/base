#!/bin/sh
set -eu

component=${1:?component required}
profile=${2:-release}
case "$component" in
    enclave)
        cargo build --release --locked --target x86_64-unknown-linux-musl \
            --package base-prover-nitro-enclave --bin base-prover-nitro-enclave
        cp target/x86_64-unknown-linux-musl/release/base-prover-nitro-enclave /app/base-prover-nitro-enclave
        exit 0 ;;
    eif-tool)
        cargo build --all --release --locked
        cp target/release/eif_build /usr/local/bin/eif_build
        exit 0 ;;
    nitro-cli)
        cargo build --release --locked
        cp target/release/nitro-cli /usr/local/bin/nitro-cli
        exit 0 ;;
    nitro-host) package=base-prover-nitro-host; binary=base-prover-nitro-host ;;
    proposer) package=base-proposer-bin; binary=base-proposer ;;
    challenger) package=base-challenger-bin; binary=base-challenger ;;
    registrar) package=base-proof-tee-registrar-bin; binary=base-proof-tee-registrar ;;
    prover-service) package=base-prover-service-bin; binary=base-prover-service ;;
    zk-host)
        package=base-prover-zk-host; binary=base-prover-zk-host
        export BASE_SUCCINCT_ELF_REQUIRE=1 ;;
    snark-e2e)
        package=base-snark-e2e-bin; binary=base-snark-e2e
        export BASE_SUCCINCT_ELF_REQUIRE=1 ;;
    challenger-e2e) package=base-challenger-e2e-bin; binary=base-challenger-e2e; profile=release ;;
    *) echo "unknown build component: $component" >&2; exit 1 ;;
esac
cargo build --profile "$profile" --package "$package" --bin "$binary"
if [ "$profile" = dev ]; then profile=debug; fi
cp "/app/target/$profile/$binary" "/app/$binary"
