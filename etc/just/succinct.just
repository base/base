_cargo_prove := "~/.sp1/bin/cargo-prove"
_sp1_tag := "v6.1.0"
_repo_root := justfile_directory()
_succinct_dir := justfile_directory() / "crates/proof/succinct"

# Show available succinct commands
default:
    @just --justfile {{ source_file() }} --list

# Ensure the ELF cache matches `elf/manifest.toml`. No-op on hit; rebuilds on miss.
# Prepend this to any recipe that compiles `base-proof-succinct-elfs` so local and CI
# builds pick up the ELFs just like `just build::contracts` handles contracts.
ensure-elfs:
    #!/usr/bin/env bash
    set -euo pipefail
    repo="{{ _repo_root }}"
    here="{{ _succinct_dir }}"
    script="$repo/etc/scripts/local/check-elf-manifest.py"
    manifest="$here/elf/manifest.toml"
    cache_dir="$here/elf"
    status=$(python3 "$script" "$manifest" "$cache_dir" || true)
    case "$status" in
        match) echo "ELF cache up to date (manifest matches)";;
        *)
            echo "ELF cache stale or missing ($status); rebuilding..."
            ( cd "$repo" && just succinct build-elfs )
            result=$(python3 "$script" "$manifest" "$cache_dir")
            if [ "$result" != "match" ]; then
                echo "error: freshly built ELFs do not match manifest.toml ($result)." >&2
                echo "       If this is a legitimate change, run 'just succinct write-manifest' and commit." >&2
                exit 1
            fi
            ;;
    esac

# Overwrite manifest.toml's sha256 fields from the current ELF cache. Run after
# intentionally changing the SP1 programs.
write-manifest:
    #!/usr/bin/env bash
    set -euo pipefail
    repo="{{ _repo_root }}"
    here="{{ _succinct_dir }}"
    python3 "$repo/etc/scripts/local/check-elf-manifest.py" \
        --write "$here/elf/manifest.toml" "$here/elf"
    echo "Updated manifest.toml. Commit the change."

# Build all SP1 ELF files (range + aggregation)
build-elfs: build-range-elf build-agg-elf

# Build the range program ELF (reproducible docker build)
build-range-elf:
    #!/usr/bin/env bash
    set -euo pipefail
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

# Print range and aggregation verification key hashes as a markdown table
vkeys:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Generating verification key hashes..."
    ETH_OUTPUT=$(RUST_LOG=error cargo run -p base-proof-succinct-scripts --bin config --release 2>&1)
    ETH_RANGE=$(echo "$ETH_OUTPUT" | grep "Range Verification Key Hash" | awk '{print $NF}')
    AGG_KEY=$(echo "$ETH_OUTPUT" | grep "Aggregation Verification Key Hash" | awk '{print $NF}')
    echo ""
    echo "| Program | Verification Key Hash |"
    echo "|--------|------------------------|"
    echo "| Range Verification Key | $ETH_RANGE |"
    echo "| Aggregation Verification Key | $AGG_KEY |"
