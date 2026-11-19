#!/bin/bash
set -e

CONDUCTOR0_URL="${CONDUCTOR0_URL:-http://op-conductor-0:6545}"
CONDUCTOR1_URL="${CONDUCTOR1_URL:-http://op-conductor-1:6546}"
CONDUCTOR2_URL="${CONDUCTOR2_URL:-http://op-conductor-2:6547}"
CONDUCTOR1_RAFT_ADDR="${CONDUCTOR1_RAFT_ADDR:-op-conductor-1:5051}"
CONDUCTOR2_RAFT_ADDR="${CONDUCTOR2_RAFT_ADDR:-op-conductor-2:5052}"

echo "=== Conductor Cluster Setup ==="

wait_for_rpc() {
  local url="$1"
  local name="$2"
  local max_retries=120
  local count=0
  echo "Waiting for $name at $url..."
  until curl -s --max-time 2 -X POST "$url" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"conductor_leader","params":[],"id":1}' \
    >/dev/null 2>&1; do
    count=$((count + 1))
    if [ $count -ge $max_retries ]; then
      echo "ERROR: $name not ready after $max_retries retries"
      exit 1
    fi
    sleep 0.5
  done
  echo "$name is ready"
}

wait_for_rpc "$CONDUCTOR0_URL" "op-conductor-0"
wait_for_rpc "$CONDUCTOR1_URL" "op-conductor-1"
wait_for_rpc "$CONDUCTOR2_URL" "op-conductor-2"

# Sends a JSON-RPC request and prints the response. Returns non-zero if curl fails or
# the response is not JSON. An optional fourth argument overrides the timeout in seconds.
rpc_call() {
  local url="$1"
  local method="$2"
  local params="$3"
  local timeout="${4:-5}"
  local response
  response=$(curl -s --max-time "$timeout" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}") || return 1
  echo "$response" | jq -e . >/dev/null 2>&1 || return 1
  echo "$response"
}

# op-conductor-0 bootstraps the raft cluster, but membership changes are only accepted by the
# leader, so wait for it to win its election before adding voters.
wait_for_leader() {
  local url="$1"
  local name="$2"
  local max_retries=120
  local count=0
  local response
  echo "Waiting for $name to become raft leader..."
  until response=$(rpc_call "$url" conductor_leader '[]') &&
    [ "$(echo "$response" | jq -r '.result')" = "true" ]; do
    count=$((count + 1))
    if [ $count -ge $max_retries ]; then
      echo "ERROR: $name did not become raft leader after $max_retries retries"
      echo "Last response: ${response:-<none>}"
      exit 1
    fi
    sleep 0.5
  done
  echo "$name is the raft leader"
}

# Adds a server as a raft voter through op-conductor-0. Re-adding an existing voter succeeds
# in raft, so retrying after a lost response is safe.
add_voter() {
  local id="$1"
  local addr="$2"
  local max_retries=20
  local count=0
  local response
  echo "Adding $id ($addr) as raft voter..."
  until response=$(rpc_call "$CONDUCTOR0_URL" conductor_addServerAsVoter "[\"$id\",\"$addr\",0]") &&
    echo "$response" | jq -e 'has("error") | not' >/dev/null; do
    count=$((count + 1))
    if [ $count -ge $max_retries ]; then
      echo "ERROR: failed to add $id as raft voter after $max_retries attempts"
      echo "Last response: ${response:-<no response>}"
      exit 1
    fi
    echo "Adding $id failed (attempt $count/$max_retries): ${response:-<no response>}"
    sleep 1
  done
  echo "$response" | jq .
}

# Counts the voters in a conductor_clusterMembership response. op-conductor encodes suffrage
# as an integer (0 = voter); a "Voter" string is accepted too. Prints 0 for error responses.
count_voters() {
  jq -r '[(.result.servers? // [])[]? |
    select(.suffrage == 0 or ((.suffrage | tostring | ascii_downcase) == "voter"))] | length'
}

# Waits until op-conductor-0 reports exactly 3 servers in the cluster, all voters.
verify_membership() {
  local max_retries=20
  local count=0
  local response
  until response=$(rpc_call "$CONDUCTOR0_URL" conductor_clusterMembership '[]') &&
    [ "$(echo "$response" | count_voters 2>/dev/null)" = "3" ] &&
    echo "$response" | jq -e '(.result.servers | length) == 3' >/dev/null; do
    count=$((count + 1))
    if [ $count -ge $max_retries ]; then
      echo "ERROR: cluster membership is not 3 voters after $max_retries attempts"
      echo "Last response: ${response:-<no response>}"
      exit 1
    fi
    sleep 1
  done
  echo "$response" | jq .
}

# op-conductor pauses itself when it bootstraps a new raft cluster, and nothing else resumes
# it, so resume op-conductor-0 once the cluster has formed.
ensure_resumed() {
  local max_retries=20
  local count=0
  local response
  local paused
  while true; do
    paused=""
    if response=$(rpc_call "$CONDUCTOR0_URL" conductor_paused '[]'); then
      paused=$(echo "$response" | jq -r '.result')
    fi
    if [ "$paused" = "false" ]; then
      echo "op-conductor-0 is not paused"
      return 0
    fi
    count=$((count + 1))
    if [ $count -ge $max_retries ]; then
      echo "ERROR: op-conductor-0 is still paused after $max_retries attempts"
      echo "Last response: ${response:-<no response>}"
      exit 1
    fi
    if [ "$paused" = "true" ]; then
      echo "op-conductor-0 is paused, resuming..."
      if ! response=$(rpc_call "$CONDUCTOR0_URL" conductor_resume '[]') ||
        ! echo "$response" | jq -e 'has("error") | not' >/dev/null; then
        echo "Resume failed (attempt $count/$max_retries): ${response:-<no response>}"
      fi
    else
      echo "Checking paused state failed (attempt $count/$max_retries): ${response:-<no response>}"
    fi
    sleep 1
  done
}

# Warns about paused conductors without resuming them. A pause can be deliberate, so it must
# survive re-runs, but it can also be the bootstrap pause left by a first run that died before
# ensure_resumed. Unreachable conductors and error replies are logged as unknown.
warn_paused_conductors() {
  local url
  local response
  local paused
  local paused_list=""
  for url in "$CONDUCTOR0_URL" "$CONDUCTOR1_URL" "$CONDUCTOR2_URL"; do
    paused=""
    response=""
    if response=$(rpc_call "$url" conductor_paused '[]' 2); then
      paused=$(echo "$response" | jq -r '.result?' 2>/dev/null) || paused=""
    fi
    case "$paused" in
      true) paused_list="${paused_list:+$paused_list, }$url" ;;
      false) ;;
      *) echo "Paused state of $url is unknown: ${response:-<no response>}" ;;
    esac
  done
  if [ -n "$paused_list" ]; then
    echo "WARN: paused conductors: $paused_list; if this follows a failed first run, resume each listed conductor with conductor_resume on that endpoint"
  fi
}

# On a re-run against persisted raft state the cluster already exists and leadership may have
# moved off op-conductor-0, so waiting for op-conductor-0 to lead would fail. If any conductor
# reports 3 voters, leave the cluster as it is: no add-voter and no resume, so a conductor an
# operator paused on purpose stays paused. Paused conductors are reported instead.
check_existing_cluster() {
  local url
  local leader_url
  local response
  local voters
  local leader=""
  for url in "$CONDUCTOR0_URL" "$CONDUCTOR1_URL" "$CONDUCTOR2_URL"; do
    if ! response=$(rpc_call "$url" conductor_clusterMembership '[]' 2); then
      echo "Membership check on $url failed, trying the next conductor"
      continue
    fi
    voters=$(echo "$response" | count_voters 2>/dev/null) || voters=0
    if [ "$voters" = "3" ]; then
      for leader_url in "$CONDUCTOR0_URL" "$CONDUCTOR1_URL" "$CONDUCTOR2_URL"; do
        if response=$(rpc_call "$leader_url" conductor_leaderWithID '[]' 2); then
          leader=$(echo "$response" | jq -r '.result.id? // empty' 2>/dev/null) || leader=""
          if [ -n "$leader" ]; then
            break
          fi
        fi
      done
      echo "cluster already formed; leader is ${leader:-unknown}"
      warn_paused_conductors
      echo ""
      echo "=== Conductor cluster setup complete ==="
      exit 0
    fi
    echo "Membership on $url has $voters voter(s): $response"
  done
}

echo ""
echo "=== Checking for an existing cluster ==="
check_existing_cluster

echo ""
echo "=== Waiting for op-conductor-0 leadership ==="
wait_for_leader "$CONDUCTOR0_URL" "op-conductor-0"

echo ""
echo "=== Adding sequencer-1 as Raft voter ==="
add_voter "sequencer-1" "$CONDUCTOR1_RAFT_ADDR"

echo ""
echo "=== Adding sequencer-2 as Raft voter ==="
add_voter "sequencer-2" "$CONDUCTOR2_RAFT_ADDR"

echo ""
echo "=== Verifying cluster membership ==="
verify_membership

echo ""
echo "=== Ensuring op-conductor-0 is resumed ==="
ensure_resumed

echo ""
echo "=== Conductor cluster setup complete ==="
