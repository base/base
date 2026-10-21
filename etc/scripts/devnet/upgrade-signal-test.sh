#!/usr/bin/env bash
# Regression checks using real filesystem permissions and Docker, with offline RPC/tool stubs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ "$(id -u)" == 0 ]]; then
  echo "Run as a non-root user to exercise Docker-owned config permissions." >&2
  exit 1
fi
TEST_DIR="$(mktemp -d)"
trap 'docker run --rm -v "$TEST_DIR:/test" alpine sh -c "rm -rf /test/*"; rmdir "$TEST_DIR"' EXIT
mkdir -p "$TEST_DIR/bin" "$TEST_DIR/user configs"
cat >"$TEST_DIR/bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo azul
EOF
cat >"$TEST_DIR/bin/cast" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  block-number) echo 1 ;;
  code) echo 0x1234 ;;
  send) echo '{}' ;;
  call)
    if [[ "$2" == --json ]]; then echo '[[123]]'; else echo 4294967296; fi
    ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$TEST_DIR/bin/"*
export PATH="$TEST_DIR/bin:$PATH"
export UPGRADE_SIGNAL_CONTRACT=0x1111111111111111111111111111111111111111
export UPGRADE_SIGNAL_CONTAINER_L1_RPC=http://l1-el:4545
export UPGRADE_SIGNAL_MODE=runtime-admin UPGRADE_SIGNAL_L1_BLOCK_TAG=latest
export UPGRADE_SIGNAL_ENV_FILES=
export UPGRADE_SIGNAL_ROLLUP_JSON="$TEST_DIR/rollup.json"
printf '%s\n' '{"genesis":{"l2_time":123},"base":{"azul":0}}' >"$UPGRADE_SIGNAL_ROLLUP_JSON"

# Match setup-devnet's root-owned directory and existing output file.
docker run --rm -v "$TEST_DIR:/test" alpine sh -c '
  mkdir -p "/test/root configs"
  echo stale > "/test/root configs/upgrade-signal.env"
  chmod 755 "/test/root configs"
  chmod 644 "/test/root configs/upgrade-signal.env"
'
[[ ! -w "$TEST_DIR/root configs" ]]
[[ ! -w "$TEST_DIR/root configs/upgrade-signal.env" ]]

for output in "user configs/new.env" "root configs/upgrade-signal.env" "root configs/new.env"; do
  export UPGRADE_SIGNAL_ENV_OUT="$TEST_DIR/$output"
  # Repeat to cover both creation and replacement without accumulating content.
  for attempt in 1 2; do
    bash "$SCRIPT_DIR/upgrade-signal.sh" setup >"$TEST_DIR/output"
    diff -u <(printf '%s\n' \
      "BASE_NODE_UPGRADE_SIGNAL_CONTRACT=$UPGRADE_SIGNAL_CONTRACT" \
      "BASE_NODE_UPGRADE_SIGNAL_L1_RPC=$UPGRADE_SIGNAL_CONTAINER_L1_RPC" \
      "BASE_NODE_UPGRADE_SIGNAL_MODE=$UPGRADE_SIGNAL_MODE" \
      "BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG=$UPGRADE_SIGNAL_L1_BLOCK_TAG") "$UPGRADE_SIGNAL_ENV_OUT"
  done
  echo "PASS: $output"
done
