_openvm_dir := justfile_directory() / "crates/proof/zk/programs/openvm"
_openvm_elf := _openvm_dir / "elf"

# Show available openvm commands
default:
    @just --justfile {{ source_file() }} --list

[private]
require-cargo-openvm:
    #!/usr/bin/env bash
    if ! cargo openvm --version >/dev/null 2>&1; then
        echo "cargo-openvm v2.0.2 is not installed; see the Prerequisites section of crates/proof/zk/programs/openvm/README.md" >&2
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

# Execute the range guest locally and print the revealed boot digest (defaults to the demo's last witness)
run input=(_openvm_elf / "input.json"): require-vmexe
    cd "{{ _openvm_dir }}/range/ethereum" && cargo openvm run \
        --exe "{{ _openvm_elf }}/range.vmexe" --config openvm.toml --input {{ quote(input) }}

# Open the basectl TUI and prove one zeronet block live
demo:
    cargo run -p basectl -- monitor openvm

# Remove built OpenVM artifacts
clean-elfs:
    rm -rf "{{ _openvm_elf }}"
