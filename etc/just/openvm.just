_openvm_dir := justfile_directory() / "crates/proof/zk/programs/openvm"
_openvm_elf := _openvm_dir / "elf"
_openvm_tag := "v2.0.2"

# Show available openvm commands
default:
    @just --justfile {{ source_file() }} --list

[private]
require-cargo-openvm:
    #!/usr/bin/env bash
    if ! cargo openvm --version >/dev/null 2>&1; then
        echo "cargo-openvm is not installed. Run:" >&2
        echo "  rustup install nightly-2026-01-18" >&2
        echo "  rustup component add rust-src --toolchain nightly-2026-01-18" >&2
        echo "  cargo +1.91 install --locked --git https://github.com/openvm-org/openvm.git --tag {{ _openvm_tag }} cargo-openvm" >&2
        exit 1
    fi

[private]
require-vmexe: require-cargo-openvm
    #!/usr/bin/env bash
    if [ ! -s "{{ _openvm_elf }}/range.vmexe" ]; then
        echo "OpenVM range vmexe not found. Build it first: just openvm build-elf" >&2
        exit 1
    fi

# Build the OpenVM range vmexe into elf/
build-elf: require-cargo-openvm
    mkdir -p "{{ _openvm_elf }}"
    cd "{{ _openvm_dir }}/range/ethereum" && cargo openvm build --ignore-rust-version --output-dir "{{ _openvm_elf }}"

# Dump a 1-block witness from the RPCs in ~/.config/base/networks/zeronet.yaml into elf/input.json
dump-zeronet:
    #!/usr/bin/env bash
    set -euo pipefail
    cfg="${HOME}/.config/base/networks/zeronet.yaml"
    yaml_val() { awk -v k="$1:" '$1==k {print $2; exit}' "$cfg"; }
    for key in rpc l1_rpc l1_beacon_rpc consensus_node_rpc; do
        if [ -z "$(yaml_val "$key" 2>/dev/null)" ]; then
            echo "could not read $key from $cfg" >&2
            exit 1
        fi
    done
    cargo run -p base-openvm-dump -- \
        --l1-rpc "$(yaml_val l1_rpc)" \
        --l1-beacon-rpc "$(yaml_val l1_beacon_rpc)" \
        --l2-rpc "$(yaml_val rpc)" \
        --l2-node-rpc "$(yaml_val consensus_node_rpc)" \
        --out-dir "{{ _openvm_elf }}"

# Execute the range guest against an OpenVM --input file and print the revealed boot digest
run input=(_openvm_elf / "input.json"): require-vmexe
    cd "{{ _openvm_dir }}/range/ethereum" && cargo openvm run --ignore-rust-version \
        --exe "{{ _openvm_elf }}/range.vmexe" --config openvm.toml --input {{ quote(input) }}

# Dump a zeronet witness, then produce an OpenVM app proof for it
prove-zeronet: require-vmexe dump-zeronet
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ _openvm_dir }}/range/ethereum"
    common=(--ignore-rust-version --exe "{{ _openvm_elf }}/range.vmexe" --config openvm.toml)
    if [ ! -s "{{ _openvm_elf }}/app.pk" ]; then
        cargo openvm keygen "${common[@]}" --app-only \
            --app-pk "{{ _openvm_elf }}/app.pk" --app-vk "{{ _openvm_elf }}/app.vk"
    fi
    cargo openvm prove app "${common[@]}" \
        --input "{{ _openvm_elf }}/input.json" \
        --app-pk "{{ _openvm_elf }}/app.pk" \
        --proof "{{ _openvm_elf }}/range.app.proof"

# Open the basectl TUI and prove one zeronet block live
demo:
    cargo run -p basectl -- monitor openvm

# Remove built OpenVM artifacts
clean-elfs:
    rm -rf "{{ _openvm_elf }}"
    rm -f "{{ _openvm_dir }}/range/ethereum/openvm_init.rs"
