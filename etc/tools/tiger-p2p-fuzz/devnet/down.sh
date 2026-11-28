#!/bin/bash
# Stops the tp2p devnet services (they are scoped to $D, so stop runs there) and deletes its data.
# Leftover devnet processes, such as ones whose service could not be stopped, are killed too.
D=${DEVNET_DIR:-/tmp/tp2p-devnet}
[ -d $D ] && cd $D
for s in dn-sl-proxy dn-beacon dn-sl-cl dn-fol-cl dn-seq-cl dn-sl-tiger dn-fol-tiger dn-seq-tiger dn-anvil; do amp orb service stop $s >/dev/null 2>&1; done
pids=$(pgrep -f "datadir $D/|l2-config-file $D/|anvil --port 8545|beacon-stub.py|rpc-count.py")
[ -n "$pids" ] && kill $pids 2>/dev/null; sleep 1
pids=$(pgrep -f "datadir $D/|l2-config-file $D/|anvil --port 8545|beacon-stub.py|rpc-count.py")
[ -n "$pids" ] && kill -9 $pids 2>/dev/null
cd /; rm -rf $D
