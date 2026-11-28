#!/bin/bash
# tp2p acceptance devnet: anvil L1, then three base-consensus + tiger pairs, all with libp2p and
# devp2p off, talking over tp2p only:
#   seq: base-consensus sequencer + tiger (builds blocks, serves witnesses)
#   fol: base-consensus validator + full tiger (unsafe blocks over tp2p gossip, EL sync over tp2p)
#   sl:  base-consensus validator + stateless tiger (witnesses from tiger peers over tp2p)
# Each process runs as an amp orb service; logs in $D/logs. Run down.sh first for a fresh start.
set -euo pipefail
D=${DEVNET_DIR:-/tmp/tp2p-devnet}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
CONS=${CONS:-$ROOT/target/release/base-consensus}   # built with --features tp2p
TIGER=${TIGER:-$ROOT/tiger/target/release/tiger}   # built with --features tp2p
ANVIL=${ANVIL:-$(command -v anvil)}
CAST=${CAST:-cast}
mkdir -p $D/logs
cd $D

L1_CHAIN=1
L2_CHAIN=84539
SEQ_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcaf784d7bf4f2ff80   # anvil key 0, throwaway
SEQ_ADDR=$($CAST wallet address --private-key $SEQ_KEY)
SYSCFG=0x5c00000000000000000000000000000000000000
SIGNER_SLOT=0x65a7ed542fb37fe237fdfbdd70b31598523fe5b32879e307bae27a0bd9581c08

svc() { # name, command
  amp orb service start "$1" --command "bash -lc 'exec $2 >> $D/logs/$1.log 2>&1'" >/dev/null
}

# L1
svc dn-anvil "$ANVIL --port 8545 --chain-id $L1_CHAIN --block-time 2 --silent"
until $CAST block-number --rpc-url http://127.0.0.1:8545 >/dev/null 2>&1; do sleep 0.2; done
$CAST rpc --rpc-url http://127.0.0.1:8545 anvil_setStorageAt $SYSCFG $SIGNER_SLOT \
  0x000000000000000000000000${SEQ_ADDR:2} >/dev/null
L1_JSON=$($CAST rpc --rpc-url http://127.0.0.1:8545 eth_getBlockByNumber latest false)
L1_HASH=$(echo "$L1_JSON" | jq -r .hash)
L1_NUM=$(( $(echo "$L1_JSON" | jq -r .number) ))
L1_TIME=$(( $(echo "$L1_JSON" | jq -r .timestamp) ))
L2_TIME=$(( L1_TIME + 2 ))
L1_GENESIS_TIME=$(( $($CAST rpc --rpc-url http://127.0.0.1:8545 eth_getBlockByNumber 0x0 false | jq -r .timestamp) ))
svc dn-beacon "python3 $HERE/beacon-stub.py $L1_GENESIS_TIME 5052"

# L2 genesis (tiger and base-reth format): every upgrade through Jovian at genesis. The L1Block
# predeploy holds `SSTORE(NUMBER, TIMESTAMP)`, so every block's L1 info deposit writes a new slot
# and changes the state root (tiger builds no user transactions).
cat > genesis.json <<EOF
{
  "config": {
    "chainId": $L2_CHAIN, "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
    "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0, "istanbulBlock": 0,
    "muirGlacierBlock": 0, "berlinBlock": 0, "londonBlock": 0, "arrowGlacierBlock": 0,
    "grayGlacierBlock": 0, "mergeNetsplitBlock": 0, "bedrockBlock": 0, "regolithTime": 0,
    "canyonTime": 0, "deltaTime": 0, "ecotoneTime": 0, "fjordTime": 0, "graniteTime": 0,
    "holoceneTime": 0, "isthmusTime": 0, "jovianTime": 0, "shanghaiTime": 0, "cancunTime": 0, "pragueTime": 0,
    "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
    "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50, "eip1559DenominatorCanyon": 250 }
  },
  "nonce": "0x0", "timestamp": "$(printf '0x%x' $L2_TIME)", "extraData": "0x0100000032000000060000000000000000",
  "gasLimit": "0x2faf080", "difficulty": "0x0",
  "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "coinbase": "0x4200000000000000000000000000000000000011", "baseFeePerGas": "0x3b9aca00",
  "excessBlobGas": "0x0", "blobGasUsed": "0x0",
  "alloc": {
    "4200000000000000000000000000000000000015": { "balance": "0x0", "code": "0x43425500" },
    "$SEQ_ADDR": { "balance": "0x3635c9adc5dea00000" }
  }
}
EOF
openssl rand -hex 32 > jwt.hex

# Sequencer tiger first, to read the L2 genesis hash.
mkdir -p seq fol sl
for n in seq fol sl; do [ -f $n/key ] || openssl rand -hex 32 > $n/key; done
tp2p_key() { # compressed secp256k1 public key (tp2p://) of a hex secret file
  local pub; pub=$($CAST wallet public-key --raw-private-key "0x$(cat $1)")
  python3 -c "import sys; p=sys.argv[1][2:]; print(('03' if int(p[64:],16)&1 else '02')+p[:64])" "$pub"
}

T_SEQ="--chain $D/genesis.json --datadir $D/seq/tiger --http --http.api eth,debug,net,web3 --http.port 9545 --authrpc.port 9551 --authrpc.jwtsecret $D/jwt.hex --p2p-secret-key $D/seq/key --tp2p.listen 127.0.0.1:9600 --tp2p.serve-witness --tp2p.only"
svc dn-seq-tiger "$TIGER node $T_SEQ"
until curl -s -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x0",false]}' http://127.0.0.1:9545 | jq -e .result.hash >/dev/null 2>&1; do sleep 0.3; done
L2_HASH=$(curl -s -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x0",false]}' http://127.0.0.1:9545 | jq -r .result.hash)

cat > rollup.json <<EOF
{
  "genesis": {
    "l1": { "hash": "$L1_HASH", "number": $L1_NUM },
    "l2": { "hash": "$L2_HASH", "number": 0 },
    "l2_time": $L2_TIME,
    "system_config": {
      "batcherAddr": "0x00000000000000000000000000000000000000b0",
      "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
      "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
      "gasLimit": 50000000, "baseFeeScalar": 0, "blobBaseFeeScalar": 0,
      "eip1559Denominator": 50, "eip1559Elasticity": 6,
      "operatorFeeScalar": 0, "operatorFeeConstant": 0, "minBaseFee": 0, "daFootprintGasScalar": 0
    }
  },
  "block_time": 2, "max_sequencer_drift": 600, "seq_window_size": 3600, "channel_timeout": 300,
  "l1_chain_id": $L1_CHAIN, "l2_chain_id": $L2_CHAIN,
  "regolith_time": 0, "canyon_time": 0, "delta_time": 0, "ecotone_time": 0, "fjord_time": 0,
  "granite_time": 0, "holocene_time": 0, "isthmus_time": 0, "jovian_time": 0,
  "batch_inbox_address": "0xff00000000000000000000000000000000084539",
  "deposit_contract_address": "0x00000000000000000000000000000000000000d0",
  "l1_system_config_address": "$SYSCFG",
  "protocol_versions_address": "0x0000000000000000000000000000000000000000",
  "chain_op_config": { "eip1559Elasticity": 6, "eip1559Denominator": 50, "eip1559DenominatorCanyon": 250 }
}
EOF

for n in seq fol sl; do [ -f $n/cl-key ] || openssl rand -hex 32 > $n/cl-key; done
SEQ_T=$(tp2p_key seq/key); FOL_T=$(tp2p_key fol/key)
SEQ_C=$(tp2p_key seq/cl-key); FOL_C=$(tp2p_key fol/cl-key)

# Follower and stateless tigers (EL over tp2p only).
svc dn-fol-tiger "$TIGER node --chain $D/genesis.json --datadir $D/fol/tiger --http --http.port 9645 --authrpc.port 9651 --authrpc.jwtsecret $D/jwt.hex --p2p-secret-key $D/fol/key --tp2p.listen 127.0.0.1:9700 --tp2p.bootnodes tp2p://$SEQ_T@127.0.0.1:9600 --tp2p.serve-witness --tp2p.only"
svc dn-sl-proxy "python3 $HERE/rpc-count.py 9546 http://127.0.0.1:9545 $D/logs/sl-upstream-methods"
SL_TIGER="$TIGER node --chain $D/genesis.json --datadir $D/sl/tiger --http --http.port 9745 --authrpc.port 9751 --authrpc.jwtsecret $D/jwt.hex --p2p-secret-key $D/sl/key --stateless.upstream http://127.0.0.1:9546 --stateless.start latest --tp2p.listen 127.0.0.1:9800 --tp2p.bootnodes tp2p://$SEQ_T@127.0.0.1:9600,tp2p://$FOL_T@127.0.0.1:9700 --tp2p.only"

# Consensus nodes (CL gossip over tp2p only; libp2p and discv5 not started).
CL_COMMON="--l1-eth-rpc http://127.0.0.1:8545 --l1-beacon http://127.0.0.1:5052 --l1.slot-duration-override 2 --l2-config-file $D/rollup.json --l2-engine-jwt-secret $D/jwt.hex --p2p.tp2p only"
svc dn-seq-cl "$CONS node --mode sequencer $CL_COMMON --l2-engine-rpc http://127.0.0.1:9551 --rpc.port 9547 --checkpoint.path $D/seq/checkpoint --p2p.sequencer.key $SEQ_KEY --sequencer.l1-confs 0 --p2p.tp2p.listen 127.0.0.1:9620 --p2p.tp2p.priv-path $D/seq/cl-key"
svc dn-fol-cl "$CONS node --mode validator $CL_COMMON --l2-engine-rpc http://127.0.0.1:9651 --rpc.port 9647 --checkpoint.path $D/fol/checkpoint --p2p.tp2p.listen 127.0.0.1:9720 --p2p.tp2p.priv-path $D/fol/cl-key --p2p.tp2p.bootnodes tp2p://$SEQ_C@127.0.0.1:9620"
# The stateless tiger starts once the sequencer has blocks (its start block must be above genesis):
# the start block comes from the sequencer tiger's RPC, every witness after it from tp2p peers.
bn() { local h; h=$(curl -s -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' http://127.0.0.1:9545 | jq -r '.result // "0x0"'); echo $(( h )); }
for _ in $(seq 120); do [ "$(bn)" -gt 4 ] && break; sleep 1; done
[ "$(bn)" -gt 4 ] || { echo "sequencer not producing blocks; see $D/logs" >&2; exit 1; }
svc dn-sl-tiger "$SL_TIGER"
sleep 2
svc dn-sl-cl "$CONS node --mode validator $CL_COMMON --l2-engine-rpc http://127.0.0.1:9751 --rpc.port 9747 --checkpoint.path $D/sl/checkpoint --p2p.tp2p.listen 127.0.0.1:9820 --p2p.tp2p.priv-path $D/sl/cl-key --p2p.tp2p.bootnodes tp2p://$SEQ_C@127.0.0.1:9620,tp2p://$FOL_C@127.0.0.1:9720"
echo "devnet up in $D (L1 $L1_NUM $L1_HASH, L2 genesis $L2_HASH)"
