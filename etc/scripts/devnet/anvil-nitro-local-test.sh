#!/usr/bin/env bash
# Exercise bootstrap against an Anvil genesis that differs from the offline generator.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT
mkdir -p "$TEST_DIR/etc/scripts/devnet" "$TEST_DIR/etc/docker" \
  "$TEST_DIR/.devnet/l2/configs" "$TEST_DIR/bin"
cp "$SCRIPT_DIR/anvil-nitro-local.sh" "$TEST_DIR/etc/scripts/devnet/"
cp "$SCRIPT_DIR/../../docker/devnet-env" "$TEST_DIR/etc/docker/"
export PATH="$TEST_DIR/bin:$PATH"
export ANVIL_GENESIS_HASH=0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

cat >"$TEST_DIR/bin/cast" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  'chain-id '*) echo 1337 ;;
  *'eth_getBlockByNumber latest false') echo '{"number":"0x1","timestamp":"0x70"}' ;;
  *'eth_getBlockByNumber 0x0 false')
    printf '{"hash":"%s","number":"0x0","timestamp":"0x64"}\n' "$ANVIL_GENESIS_HASH"
    ;;
  *'debug_getRawHeader latest') echo '"0x1234"' ;;
  *'debug_getRawReceipts latest') echo '[]' ;;
  *) echo "Unexpected cast call: $*" >&2; exit 1 ;;
esac
EOF
cat >"$TEST_DIR/bin/curl" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *'/eth/v1/config/spec') echo '{"data":{"SECONDS_PER_SLOT":"12"}}' ;;
  *'/eth/v1/beacon/genesis') echo '{"data":{"genesis_time":"100"}}' ;;
  *) exit 1 ;;
esac
EOF
# Stop before contract fetching/deployment. No network or live chain writes occur.
printf '#!/usr/bin/env bash\nexit 73\n' >"$TEST_DIR/bin/git"
for tool in forge just docker cargo; do
  printf '#!/usr/bin/env bash\nexit 1\n' >"$TEST_DIR/bin/$tool"
done
chmod +x "$TEST_DIR/bin/"*

ROLLUP="$TEST_DIR/.devnet/l2/configs/rollup.json"
cat >"$ROLLUP" <<'EOF'
{"l1_chain_id":1337,"genesis":{"l1":{"hash":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","number":0},"l2":{"hash":"0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","number":0},"l2_time":100},"block_time":2,"base":{"denim":150}}
EOF
cp "$ROLLUP" "$TEST_DIR/original.json"
# setup-devnet creates read-only-to-the-caller files in a writable config directory.
chmod 444 "$ROLLUP"

for attempt in 1 2; do
  result=0
  bash "$TEST_DIR/etc/scripts/devnet/anvil-nitro-local.sh" bootstrap \
    >"$TEST_DIR/output" 2>&1 || result=$?
  if [[ "$result" != 73 ]]; then
    cat "$TEST_DIR/output" >&2
    echo "Bootstrap failed before contract preparation: $result" >&2
    exit 1
  fi
  jq -e --arg hash "$ANVIL_GENESIS_HASH" '.genesis.l1 == {hash: $hash, number: 0}' \
    "$ROLLUP" >/dev/null || {
      echo "FAIL: bootstrap left the offline L1 genesis hash in rollup.json" >&2
      exit 1
    }
  diff -u <(jq -S 'del(.genesis.l1.hash)' "$TEST_DIR/original.json") \
    <(jq -S 'del(.genesis.l1.hash)' "$ROLLUP")
  diff -u "$ROLLUP" "$TEST_DIR/.devnet/anvil-no-nitro/rollup.json"
done

# A malformed RPC identity must not overwrite the last usable node config.
cp "$ROLLUP" "$TEST_DIR/valid.json"
result=0
ANVIL_GENESIS_HASH='' bash "$TEST_DIR/etc/scripts/devnet/anvil-nitro-local.sh" bootstrap \
  >"$TEST_DIR/output" 2>&1 || result=$?
[[ "$result" != 0 && "$result" != 73 ]]
cmp "$ROLLUP" "$TEST_DIR/valid.json"
[[ ! -e "$TEST_DIR/.devnet/anvil-no-nitro/rollup.json" ]]
echo "PASS: live Anvil genesis reaches nodes and proofs without changing the L2 genesis or schedule"
