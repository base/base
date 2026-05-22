set positional-arguments := true

_script := justfile_directory() / "etc/scripts/zk-prover/grpc.sh"

# Show ZK prover request commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  zk-prover::'

# List services exposed by the configured ZK prover endpoint
list target='devnet':
    bash "{{ _script }}" list "{{ target }}"

# Describe the ZK prover gRPC service
describe target='devnet':
    bash "{{ _script }}" describe "{{ target }}"

# Request a proof or dry-run for a block range
prove start_block number_of_blocks='1' target='devnet' proof_type='PROOF_TYPE_COMPRESSED':
    bash "{{ _script }}" prove "{{ start_block }}" "{{ number_of_blocks }}" "{{ target }}" "{{ proof_type }}"

# Get proof status and, for dry-run mode, executionStats
get session_id target='devnet' receipt_type='':
    bash "{{ _script }}" get "{{ session_id }}" "{{ target }}" "{{ receipt_type }}"

# List recent proof requests
list-proofs target='devnet' limit='20' offset='0' status_filter='':
    bash "{{ _script }}" list-proofs "{{ target }}" "{{ limit }}" "{{ offset }}" "{{ status_filter }}"
