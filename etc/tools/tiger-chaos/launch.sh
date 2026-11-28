#!/bin/bash
# Node launcher for base-tiger-chaos: launch.sh <name> <datadir>. genesis.json, jwt.hex and
# launch.env (PORT_OFFSET, TIGER_BIN, BASE_NODE_BIN, TIGER_EXTRA) are in the datadir's parent.
#   tiger    tiger on <datadir>; BASE_DIFF_FAULT becomes TIGER_FAULT.
#   base     base-reth on <datadir> (recording), ports 18545/18551 + PORT_OFFSET.
#   handoff  base-reth on a copy of tiger's datadir, ports 19545/19551 + PORT_OFFSET.
set -e
name=$1; dir=$2; root=$(dirname "$dir")
[ -f "$root/launch.env" ] && . "$root/launch.env"
O=${PORT_OFFSET:-200}
G=$root/genesis.json; J=$root/jwt.hex
base() {
  [ -d "$dir/db" ] || "$BASE_NODE_BIN" init --chain "$G" --datadir "$dir" >/dev/null 2>&1
  exec "$BASE_NODE_BIN" node --chain "$G" --datadir "$dir" --http --http.addr 127.0.0.1 --http.port $(($1+O)) \
    --http.api eth,net,web3,debug --authrpc.addr 127.0.0.1 --authrpc.port $(($2+O)) --authrpc.jwtsecret "$J" \
    --rpc.eth-proof-window 100000 --disable-discovery --port $(($3+O)) --ipcdisable
}
case $name in
  base) base 18545 18551 30313 ;;
  handoff) base 19545 19551 30333 ;;
  tiger)
    if [ -n "${BASE_DIFF_FAULT:-}" ]; then export TIGER_FAULT=$BASE_DIFF_FAULT; echo "launch: TIGER_FAULT=$TIGER_FAULT" >&2; fi
    exec "$TIGER_BIN" node --chain "$G" --datadir "$dir" --http --http.addr 127.0.0.1 --http.port $((28545+O)) \
      --http.api eth,net,web3,debug --authrpc.addr 127.0.0.1 --authrpc.port $((28551+O)) --authrpc.jwtsecret "$J" \
      --rpc.eth-proof-window 100000 --disable-discovery --port $((30323+O)) ${TIGER_EXTRA:-} ;;
  *) echo "unknown node $name" >&2; exit 1 ;;
esac
