# Local Devnet Transaction Inclusion

## Contract

Start Base's real single-sequencer local devnet. Send a small ETH transfer from a prefunded development account directly to the sequencer/builder RPC. Require a successful receipt, then query its block by number and confirm the receipt's block hash and submitted transaction hash are both present.

This proves **sequencer inclusion**, not L1 publication/finality, validator synchronization, HA failover, or production-network behavior. A submitted hash or a nonzero block height alone is not a pass.

## Safety and prerequisites

- Use a Base checkout with disposable devnet data, not an existing valuable devnet. `up-single` first deletes the previous devnet state; `down` deletes it again.
- Use Linux with Bash, Git, Docker Compose/Buildx, `just`, `cast`, `jq`, and GNU `timeout`. Confirm the tools are installed and `docker info` succeeds before starting. Docker builds the node and setup images from source. Initial builds can take a long time.
- Inspect `etc/docker/Justfile`, `etc/docker/devnet-env`, and `etc/docker/docker-compose.yml` before running. Those are the source of truth if commands change.
- Inspect `docker ps -a` and host listeners first. The devnet uses fixed container names, subnet `172.30.0.0/24`, and ports including 4545, 7545, 8545, 8645, 8090, 3000, and 9090. Inspect all published ports with `docker compose --env-file etc/docker/devnet-env -f etc/docker/docker-compose.yml config` from the repository root. Do not collide with or shut down an existing stack. A different Compose project name does **not** make simultaneous stacks safe. Compose publishes ports on all interfaces by default; use an isolated development machine with appropriate network restrictions.
- These commands use only the published local test accounts, never a real wallet. Do not point them at a public or remote RPC. The defaults below expect local chain ID `84538453` and direct sequencer RPC port `7545`, not the validator port `8545`.

Run the blocks below in the **same Bash session**, starting at the Base repository root. Do not continue after a failing block.

If an unrelated service occupies a metrics port, use Base's existing environment override rather than stopping it or editing product code. For example, after confirming port 18090 is free, `export L2_CLIENT_METRICS_PORT=18090` before startup avoids a collision on 8090. Record any overrides with the evidence; keep the sequencer RPC and chain ID unchanged.

## 1. Inspect and start

```bash
set -euo pipefail
export COMPOSE_PROJECT_NAME=base-verifier

mkdir -p .tmp/verifier
RUN_DIR=$(mktemp -d "$PWD/.tmp/verifier/run.XXXXXX")
git rev-parse HEAD > "$RUN_DIR/base-revision.txt"
git status --short > "$RUN_DIR/base-status.txt"

# DESTRUCTIVE to this disposable checkout's devnet data; builds current source.
just devnet up-single 2>&1 | tee "$RUN_DIR/startup.log"
```

The recipe builds images before starting Compose. Do not replace it with an old `base:local` image when claiming to test a code change. If startup fails, capture logs and stop here. Evidence is saved in the ignored `.tmp/verifier/` directory, outside the devnet data deleted by cleanup.

## 2. Wait for the sequencer

```bash
# Inspect this tracked file before sourcing it. It contains PUBLIC local test keys.
source etc/docker/devnet-env
test "$L2_CHAIN_ID" = 84538453
test "$L2_BUILDER_HTTP_PORT" = 7545
RPC="http://127.0.0.1:$L2_BUILDER_HTTP_PORT"

ready=false
for attempt in $(seq 1 90); do
  chain=$(cast chain-id --rpc-url "$RPC" --rpc-timeout 2 2>/dev/null || true)
  height=$(cast block-number --rpc-url "$RPC" --rpc-timeout 2 2>/dev/null || true)
  if [[ "$chain" = "$L2_CHAIN_ID" && "$height" =~ ^[0-9]+$ ]] && (( height > 0 )); then
    ready=true
    break
  fi
  sleep 2
done
if [[ "$ready" != true ]]; then
  echo "FAIL: local sequencer did not become ready; inspect devnet logs" >&2
  exit 1
fi
printf '%s\n' "$chain" > "$RUN_DIR/chain-id.txt"
cast balance "$ANVIL_ACCOUNT_1_ADDR" --rpc-url "$RPC" --rpc-timeout 5
```

The bounded readiness loop checks both chain identity and that L2 has produced a block. A connection failure, wrong chain, or timeout must not be treated as successful verification.

## 3. Send and assert inclusion

```bash
TX=$(timeout 60s cast send "$ANVIL_ACCOUNT_2_ADDR" \
  --value 0.001ether --private-key "$ANVIL_ACCOUNT_1_KEY" \
  --chain-id "$L2_CHAIN_ID" --rpc-url "$RPC" --rpc-timeout 5 --async)
[[ "$TX" =~ ^0x[[:xdigit:]]{64}$ ]]
printf '%s\n' "$TX" > "$RUN_DIR/transaction-hash.txt"

timeout 90s cast receipt "$TX" --rpc-url "$RPC" --rpc-timeout 5 --json \
  > "$RUN_DIR/receipt.json"
jq -e --arg tx "$TX" --arg from "$ANVIL_ACCOUNT_1_ADDR" --arg to "$ANVIL_ACCOUNT_2_ADDR" '
  (.transactionHash | ascii_downcase) == ($tx | ascii_downcase) and
  (.from | ascii_downcase) == ($from | ascii_downcase) and
  (.to | ascii_downcase) == ($to | ascii_downcase) and
  (.status == "0x1" or .status == 1) and
  .blockNumber != null and .blockHash != null
' "$RUN_DIR/receipt.json"

BLOCK_NUMBER=$(jq -er '.blockNumber' "$RUN_DIR/receipt.json")
BLOCK_HASH=$(jq -er '.blockHash' "$RUN_DIR/receipt.json")
cast block "$BLOCK_NUMBER" --rpc-url "$RPC" --rpc-timeout 5 --json \
  > "$RUN_DIR/block.json"
jq -e --arg block "$BLOCK_HASH" --arg tx "$TX" '
  (.hash | ascii_downcase) == ($block | ascii_downcase) and
  any(.transactions[]; (ascii_downcase == ($tx | ascii_downcase)))
' "$RUN_DIR/block.json"

printf 'PASS: transaction %s included successfully in block %s (%s)\nEvidence: %s\n' \
  "$TX" "$BLOCK_NUMBER" "$BLOCK_HASH" "$RUN_DIR"
```

A reverted receipt, missing receipt, mismatched block, absent transaction, malformed response, or timeout is a failure. Preserve the raw response and diagnose it; never remove an assertion merely to obtain a pass. The transaction submission is not automatically retried after a timeout, because it may already have been broadcast.

## 4. Diagnose and clean up

Inspect only the owned stack from this checkout:

```bash
just devnet ps -a
just devnet logs --follow=false --no-color --tail 100 base-builder l1-el l1-cl \
  > "$RUN_DIR/devnet.log" 2>&1

# Deletes this run's devnet state. Evidence survives in .tmp/verifier/.
just devnet down
```

If a failed block exited the shell, reopen Bash at the repository root and restore `COMPOSE_PROJECT_NAME` and the existing `RUN_DIR` before cleanup. Do not start a new devnet or delete evidence to hide the failure. Verify the owned containers stopped; never use `docker system prune` or stop another project's containers.

Report the Base revision and dirty status, RPC/chain ID, transaction hash, receipt status, block number/hash, assertion results, evidence directory, and whether cleanup completed. When verifying a PR, include that evidence and state that this scenario establishes only sequencer inclusion.
