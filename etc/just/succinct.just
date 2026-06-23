_cargo_prove := "~/.sp1/bin/cargo-prove"
_sp1_tag := "v6.2.3"
_repo_root := justfile_directory()
_succinct_dir := justfile_directory() / "crates/proof/succinct"

# Show available succinct commands
default:
    @just --justfile {{ source_file() }} --list

# Ensure the ELF cache has the required SP1 ELFs. No-op on hit; rebuilds on miss.
# Prepend this to any recipe that compiles `base-proof-succinct-elfs` so local and CI
# builds pick up the ELFs just like `just build::contracts` handles contracts.
ensure-elfs:
    #!/usr/bin/env bash
    set -euo pipefail
    repo="{{ _repo_root }}"
    here="{{ _succinct_dir }}"
    cache_dir="$here/elf"
    missing=()
    for elf in range-elf-embedded aggregation-elf; do
        if [ ! -s "$cache_dir/$elf" ]; then
            missing+=("$elf")
        fi
    done
    if [ "${#missing[@]}" -eq 0 ]; then
        echo "ELF cache has required artifacts"
        exit 0
    fi
    echo "ELF cache missing required artifact(s): ${missing[*]}; rebuilding..."
    ( cd "$repo" && just succinct build-elfs )
    for elf in range-elf-embedded aggregation-elf; do
        if [ ! -s "$cache_dir/$elf" ]; then
            echo "error: missing ELF after rebuild: $cache_dir/$elf" >&2
            exit 1
        fi
    done

# Build all SP1 ELF files (range + aggregation)
build-elfs: build-range-elf build-agg-elf

# Build the range program ELF (reproducible docker build)
build-range-elf:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{ _succinct_dir }}/elf"
    cd "{{ _succinct_dir }}/programs/range/ethereum"
    {{ _cargo_prove }} prove build \
        --elf-name range-elf-embedded \
        --tag {{ _sp1_tag }} \
        --docker \
        --workspace-directory {{ _repo_root }} \
        --output-directory {{ _succinct_dir }}/elf

# Build the aggregation program ELF (reproducible docker build)
build-agg-elf:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{ _succinct_dir }}/elf"
    cd "{{ _succinct_dir }}/programs/aggregation"
    {{ _cargo_prove }} prove build \
        --elf-name aggregation-elf \
        --tag {{ _sp1_tag }} \
        --docker \
        --workspace-directory {{ _repo_root }} \
        --output-directory {{ _succinct_dir }}/elf

# Remove built ELF artifacts
clean-elfs:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    here="{{ _succinct_dir }}"
    rm -f "$here/elf/range-elf-embedded" "$here/elf/aggregation-elf" "$here/elf"/*.bin

# Print range and aggregation verification key hashes as a markdown table.
vkeys mode="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ mode }}" in
        "")
            just succinct ensure-elfs
            ;;
        --build)
            just succinct clean-elfs
            just succinct build-elfs
            ;;
        *)
            echo "usage: just succinct vkeys [--build]" >&2
            exit 2
            ;;
    esac
    echo "Generating verification key hashes..."
    ETH_OUTPUT=$(BASE_SUCCINCT_ELF_REQUIRE=1 RUST_LOG=error cargo run -p base-proof-succinct-scripts --bin vkeys --release 2>&1)
    ETH_RANGE=$(echo "$ETH_OUTPUT" | grep "Range Verification Key Hash" | awk '{print $NF}')
    AGG_KEY=$(echo "$ETH_OUTPUT" | grep "Aggregation Verification Key Hash" | awk '{print $NF}')
    echo ""
    echo "| Program | Verification Key Hash |"
    echo "|--------|------------------------|"
    echo "| Range Verification Key | $ETH_RANGE |"
    echo "| Aggregation Verification Key | $AGG_KEY |"
