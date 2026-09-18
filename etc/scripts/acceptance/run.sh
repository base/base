#!/usr/bin/env bash
# Run exactly one ignored real-client scenario; never reuse results or retry.
set -euo pipefail

case "${1:-}" in
  calldata|blob) mode="$1" ;;
  *) echo "usage: $0 calldata|blob" >&2; exit 2 ;;
esac
if [[ $# -ne 1 ]]; then
  echo "usage: $0 calldata|blob" >&2
  exit 2
fi
if [[ -n "${BASE_SYSTEM_TEST_SHARED_L1_RUNTIME:-}" ]]; then
  echo "Glamsterdam acceptance requires a fresh dedicated L1, not a shared runtime" >&2
  exit 2
fi

cd "$(git rev-parse --show-toplevel)"
if [[ -n "${BASE_GLAMSTERDAM_ARTIFACTS:-}" ]]; then
  mkdir -p "$(dirname "$BASE_GLAMSTERDAM_ARTIFACTS")"
  # Refuse existing directories, even empty ones, rather than deleting caller data.
  mkdir "$BASE_GLAMSTERDAM_ARTIFACTS"
else
  BASE_GLAMSTERDAM_ARTIFACTS="$(mktemp -d "${TMPDIR:-/tmp}/base-glamsterdam-${mode}.XXXXXXXX")"
fi
BASE_GLAMSTERDAM_ARTIFACTS="$(cd "$BASE_GLAMSTERDAM_ARTIFACTS" && pwd)"
# Artifacts must not become untracked source while its build snapshot is being verified.
case "$BASE_GLAMSTERDAM_ARTIFACTS/" in
  "$(pwd)/"*)
    if ! git check-ignore -q "$BASE_GLAMSTERDAM_ARTIFACTS"; then
      echo "Use an artifact directory outside the checkout or under an ignored path" >&2
      exit 2
    fi ;;
esac
export BASE_GLAMSTERDAM_ARTIFACTS
printf 'Acceptance artifacts: %s\n' "$BASE_GLAMSTERDAM_ARTIFACTS"
exec > >(tee "$BASE_GLAMSTERDAM_ARTIFACTS/run.log") 2>&1
trap 'printf "Runner exit status: %s\n" "$?"' EXIT

pins=etc/systems/fixtures/glamsterdam.json
cp "$pins" "$BASE_GLAMSTERDAM_ARTIFACTS/client-pins.json"
{
  read -r reth_image
  read -r lighthouse_image
  read -r setup_image
} < <(python3 - "$pins" <<'PY'
import hashlib, json, pathlib, re, sys
pins = json.load(open(sys.argv[1]))
for client in ("reth", "lighthouse"):
    image = pins[client]["image"]
    if not re.fullmatch(r"[^\s]+@sha256:[0-9a-f]{64}", image):
        raise SystemExit(f"{client} must select an immutable image digest")
    print(image)
setup = pins["setup"]
if setup["image"] != "devnet-setup:glamsterdam-v1":
    raise SystemExit("unexpected fixture setup image; refusing to overwrite a system-test default")
patch = pathlib.Path("etc/scripts/devnet/glamsterdam-genesis.patch").read_bytes()
if hashlib.sha256(patch).hexdigest() != setup["patch_sha256"]:
    raise SystemExit("setup patch differs from the recorded fixture provenance")
print(setup["image"])
PY
)

# The report store is per-run too: concurrent DA modes cannot overwrite JUnit.
python3 - "$BASE_GLAMSTERDAM_ARTIFACTS" <<'PY'
import json, pathlib, sys
output = pathlib.Path(sys.argv[1])
config = pathlib.Path(".config/nextest.toml").read_text()
config += "\n[store]\ndir = " + json.dumps(str(output / "nextest")) + "\n"
(output / "nextest.toml").write_text(config)
PY

export RUST_MIN_STACK=33554432
# Retain prepared gas/nonce and batch submission details at the fork boundary.
export RUST_LOG="${RUST_LOG:-warn,base_system_tests=debug,base_tx_manager=debug,base_batcher_core=debug,base_consensus=info}"
case_name="glamsterdam::glamsterdam_${mode}"
nextest=(cargo nextest run --config-file "$BASE_GLAMSTERDAM_ARTIFACTS/nextest.toml"
  -P glamsterdam --cargo-metadata "$BASE_GLAMSTERDAM_ARTIFACTS/cargo-metadata.json"
  --binaries-metadata "$BASE_GLAMSTERDAM_ARTIFACTS/nextest-binaries.json"
  --run-ignored only --retries 0 --no-tests fail -E "test(=${case_name})")

# Preserve actual attempted commands even if a pull/build fails before provenance
# can inspect all images. This log is not a claim that the command succeeded.
build_commands=()
prepare() {
  local rendered
  printf -v rendered '%q ' "$@"
  printf '%s\n' "$rendered" | tee -a "$BASE_GLAMSTERDAM_ARTIFACTS/build-commands.log" >&2
  build_commands+=(--build-command "$rendered")
  "$@"
}

# Everything expensive completes before the test resolves genesis/fork timestamps.
python3 etc/scripts/acceptance/capture-provenance.py snapshot "$BASE_GLAMSTERDAM_ARTIFACTS"
prepare docker pull "$reth_image"
prepare docker pull "$lighthouse_image"
prepare docker build --load -f etc/docker/Dockerfile.devnet -t "$setup_image" .
prepare cargo metadata --locked --no-deps --format-version 1 \
  > "$BASE_GLAMSTERDAM_ARTIFACTS/cargo-metadata.json"
prepare cargo nextest list --locked -p base-system-tests --no-default-features \
  --test acceptance --cargo-profile ci --list-type binaries-only --message-format json \
  > "$BASE_GLAMSTERDAM_ARTIFACTS/nextest-binaries.json"
python3 etc/scripts/acceptance/capture-provenance.py capture "$BASE_GLAMSTERDAM_ARTIFACTS" \
  --image "$reth_image" --image "$lighthouse_image" --image "$setup_image" \
  "${build_commands[@]}"
python3 etc/scripts/acceptance/capture-provenance.py verify "$BASE_GLAMSTERDAM_ARTIFACTS"

# Preserve failures and still check results. Rust normally owns cleanup; a killed
# test can bypass its destructors. The fallback selects only this run's exact
# ownership labels and retains diagnostics before deleting verified resource IDs.
# No signal trap: cleanup must not race a still-running nextest/test process.
# Killing this wrapper itself (especially SIGKILL) can still require manual cleanup.
printf '%q ' "${nextest[@]}" > "$BASE_GLAMSTERDAM_ARTIFACTS/test-command.log"
printf '\n' >> "$BASE_GLAMSTERDAM_ARTIFACTS/test-command.log"
run_status=0
"${nextest[@]}" || run_status=$?
cleanup_status=0
python3 etc/scripts/acceptance/cleanup.py "$BASE_GLAMSTERDAM_ARTIFACTS" \
  || cleanup_status=$?
report="$BASE_GLAMSTERDAM_ARTIFACTS/nextest/glamsterdam/test-results.xml"
if [[ -f "$report" ]]; then
  cp "$report" "$BASE_GLAMSTERDAM_ARTIFACTS/test-results.xml"
fi
validation_status=0
python3 etc/scripts/acceptance/validate-results.py "$mode" "$BASE_GLAMSTERDAM_ARTIFACTS" \
  || validation_status=$?
if [[ $run_status -ne 0 ]]; then
  exit "$run_status"
fi
if [[ $cleanup_status -ne 0 ]]; then
  exit "$cleanup_status"
fi
exit "$validation_status"
