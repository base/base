#!/bin/bash
# Acceptance: every block from the stateless start to the common head has the same hash and
# state root on seq (9545), fol (9645) and sl (9745); no libp2p or devp2p socket is open.
rpc() { curl -s -X POST -H 'content-type: application/json' --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":$3}" http://127.0.0.1:$1; }
L=${DEVNET_DIR:-/tmp/tp2p-devnet}/logs
start=$(sed -n 's/.*stateless start at block \([0-9]*\).*/\1/p' $L/dn-sl-tiger.log)
head=999999999
for p in 9545 9645 9745; do h=$(( $(rpc $p eth_blockNumber '[]' | jq -r .result) )); [ $h -lt $head ] && head=$h; echo "port $p head $h"; done
bad=0
for n in $(seq 1 $head); do
  for p in 9545 9645 9745; do
    [ $p = 9745 ] && [ $n -lt $start ] && continue
    rpc $p eth_getBlockByNumber "[\"$(printf 0x%x $n)\",false]" | jq -r '.result.hash+" "+.result.stateRoot' > /tmp/dn-$p
  done
  a=$(cat /tmp/dn-9545); [ "$(cat /tmp/dn-9645)" = "$a" ] || { echo "fol differs at $n"; bad=1; }
  if [ $n -ge $start ]; then [ "$(cat /tmp/dn-9745)" = "$a" ] || { echo "sl differs at $n"; bad=1; }; fi
done
echo "compared blocks 1..$head (stateless from $start): $([ $bad = 0 ] && echo ALL MATCH || echo MISMATCH)"
hx=$(printf 0x%x $head); echo "head block: $(rpc 9545 eth_getBlockByNumber "[\"$hx\",false]" | jq -c '.result | {number,hash,stateRoot}')"
echo "--- listening sockets of devnet processes:"
for pid in $(pgrep -f 'release/(tiger|base-consensus) node'); do
  name=$(tr '\0' ' ' </proc/$pid/cmdline | grep -o 'datadir [^ ]*\|rpc.port [0-9]*'); 
  echo "$name: $(ss -Hltnup 2>/dev/null | grep "pid=$pid," | awk '{print $1"/"$5}' | sort -u | tr '\n' ' ')"
done
udp=$(for pid in $(pgrep -f 'release/(tiger|base-consensus) node'); do ss -Hlunp | grep "pid=$pid,"; done | wc -l)
echo "UDP sockets (discv5) open: $udp"
echo "--- devp2p/libp2p mentions:"; grep -hiE 'devp2p|libp2p|discv5|gossipsub' $L/*.log | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-160 | sort | uniq -c | head
echo "--- stateless upstream RPC calls:"; sort $L/sl-upstream-methods | uniq -c
