#!/usr/bin/env bash
set -euo pipefail

readonly PYROSCOPE_URL="${PYROSCOPE_URL:-http://pyroscope:4040}"
readonly CPU_PROFILE_TYPE="process_cpu:cpu:nanoseconds:cpu:nanoseconds"
readonly SERVICES=(base-builder base-client base-rpc base-sequencer-1 base-sequencer-2)
declare -Ar HEAP_ENDPOINTS=(
    [base-builder]="http://base-builder:7090/debug/pprof/heap"
    [base-client]="http://base-client:8090/debug/pprof/heap"
    [base-rpc]="http://base-rpc:8190/debug/pprof/heap"
    [base-sequencer-1]="http://base-sequencer-1:10090/debug/pprof/heap"
    [base-sequencer-2]="http://base-sequencer-2:11090/debug/pprof/heap"
)

is_name() {
    local candidate="$1"
    [[ "$candidate" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] &&
        [[ "$candidate" != current-run ]] &&
        [[ "$candidate" != diffs ]]
}

is_service() {
    local candidate="$1"
    local service
    for service in "${SERVICES[@]}"; do
        [[ "$candidate" == "$service" ]] && return 0
    done
    return 1
}

select_pprof_sample() {
    local sample_name="$1"
    local raw_profile="$2"

    awk -v sample_name="$sample_name" '
        $0 == "Samples:" {
            in_samples = 1
            print
            next
        }
        in_samples && !have_header {
            for (column = 1; column <= NF; column++) {
                split($column, sample_type, "/")
                if (sample_type[1] == sample_name) {
                    sample_column = column
                }
            }
            if (!sample_column) {
                print "sample type not found: " sample_name > "/dev/stderr"
                exit 1
            }
            print "value/count value/count"
            have_header = 1
            next
        }
        in_samples && $0 == "Locations" {
            in_samples = 0
            print
            next
        }
        in_samples {
            separator = index($0, ": ")
            if (separator > 0) {
                values = substr($0, 1, separator - 1)
                stack = substr($0, separator + 2)
                if (stack ~ /^[0-9 ]+$/) {
                    sub(/^[[:space:]]+/, "", values)
                    value_count = split(values, sample_values, /[[:space:]]+/)
                    if (sample_column <= value_count) {
                        print sample_values[sample_column] " 0: " stack
                        next
                    }
                }
            }
        }
        { print }
    ' "$raw_profile"
}

render_pprof() {
    local service="$1"
    local kind="$2"
    local sample_index="$3"
    local count_name="$4"
    local color="$5"
    local output_dir="$6"
    local title="$service $kind flame graph"

    go tool pprof \
        -sample_index="$sample_index" \
        -raw \
        -output="$output_dir/$kind.raw" \
        "$output_dir/$kind.pprof"
    select_pprof_sample "$sample_index" "$output_dir/$kind.raw" | \
        stackcollapse-go.pl > "$output_dir/$kind.folded"
    [[ -s "$output_dir/$kind.folded" ]] || {
        echo "profile contained no stacks: $service $kind" >&2
        return 1
    }
    flamegraph.pl \
        --hash \
        --colors="$color" \
        --countname="$count_name" \
        --title="$title" \
        "$output_dir/$kind.folded" > "$output_dir/$kind.svg"
}

report() {
    : "${PROFILE_RUN_ID:?PROFILE_RUN_ID is required}"
    : "${PROFILE_FROM:?PROFILE_FROM is required}"
    : "${PROFILE_TO:?PROFILE_TO is required}"
    is_name "$PROFILE_RUN_ID" || {
        echo "invalid profiling run name: $PROFILE_RUN_ID" >&2
        return 1
    }

    local output_dir="/profiles/$PROFILE_RUN_ID"
    local requested_service="${PROFILE_SERVICE:-}"
    local selected_services=("${SERVICES[@]}")
    local service

    if [[ -n "$requested_service" ]]; then
        is_service "$requested_service" || {
            echo "unknown service: $requested_service" >&2
            return 1
        }
        selected_services=("$requested_service")
    fi

    for service in "${selected_services[@]}"; do
        [[ ! -e "$output_dir/$service" ]] || {
            echo "report already exists: $output_dir/$service" >&2
            return 1
        }
    done

    local cleanup_command
    local staging_dir
    staging_dir="$(mktemp -d "$output_dir/.report.XXXXXX")"
    printf -v cleanup_command 'rm -rf -- %q' "$staging_dir"
    # Expand now because this function-local path is unavailable when the EXIT trap runs.
    # shellcheck disable=SC2064
    trap "$cleanup_command" EXIT

    for service in "${selected_services[@]}"; do
        echo "Rendering CPU and heap profiles for $service"
        mkdir -p "$staging_dir/$service"
        profilecli query profile \
            --url="$PYROSCOPE_URL" \
            --from="$PROFILE_FROM" \
            --to="$PROFILE_TO" \
            --query="{service_name=\"$service\"}" \
            --profile-type="$CPU_PROFILE_TYPE" \
            --output="pprof=$staging_dir/$service/cpu.pprof"
        render_pprof "$service" cpu cpu nanoseconds hot "$staging_dir/$service"

        curl --fail --silent --show-error --retry 3 --retry-delay 1 \
            --output "$staging_dir/$service/heap.pprof" \
            "${HEAP_ENDPOINTS[$service]}"
        render_pprof "$service" heap inuse_space bytes mem "$staging_dir/$service"
    done

    for service in "${selected_services[@]}"; do
        mv "$staging_dir/$service" "$output_dir/$service"
    done
    rmdir "$staging_dir"
    trap - EXIT
}

diff_profiles() {
    local baseline="$1"
    local comparison="$2"
    local service="$3"
    local kind="$4"
    local baseline_profile="/profiles/$baseline/$service/$kind.folded"
    local comparison_profile="/profiles/$comparison/$service/$kind.folded"
    local output_dir="/profiles/diffs/${comparison}-vs-${baseline}/$service"
    local diff_profile="$output_dir/$kind.folded"
    local count_name
    local diff_args=()

    is_name "$baseline" || {
        echo "invalid baseline run name: $baseline" >&2
        return 1
    }
    is_name "$comparison" || {
        echo "invalid comparison run name: $comparison" >&2
        return 1
    }
    is_service "$service" || {
        echo "unknown service: $service" >&2
        return 1
    }
    case "$kind" in
        cpu)
            count_name=nanoseconds
            diff_args=(-n)
            ;;
        heap) count_name=bytes ;;
        *)
            echo "profile kind must be cpu or heap: $kind" >&2
            return 1
            ;;
    esac
    [[ -s "$baseline_profile" ]] || {
        echo "profile not found: $baseline_profile" >&2
        return 1
    }
    [[ -s "$comparison_profile" ]] || {
        echo "profile not found: $comparison_profile" >&2
        return 1
    }

    mkdir -p "$output_dir"
    difffolded.pl "${diff_args[@]}" "$baseline_profile" "$comparison_profile" > "$diff_profile"
    flamegraph.pl \
        --countname="$count_name" \
        --subtitle="Widths show $comparison; red increased, blue decreased" \
        --title="$service $kind: $comparison vs $baseline" \
        "$diff_profile" > "$output_dir/$kind.svg"
    echo "Wrote $output_dir/$kind.svg"
}

case "${1:-}" in
    report) report ;;
    diff)
        [[ $# -eq 5 ]] || {
            echo "usage: profiling-tools diff BASELINE COMPARISON SERVICE cpu|heap" >&2
            exit 1
        }
        diff_profiles "$2" "$3" "$4" "$5"
        ;;
    *)
        echo "usage: profiling-tools report|diff" >&2
        exit 1
        ;;
esac
