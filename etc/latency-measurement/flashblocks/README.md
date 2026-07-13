# Flashblocks latency sidecar

Run **alongside** each `base-consensus observe` node to decompose the CL gossip latency into
**geographic floor** vs **P2P/gossip overhead**.

- Flashblocks arrive over a single websocket hop → the us-east→APAC delta ≈ the pure
  geographic/network floor any protocol pays (and the floor a websocket fast path would hit).
- CL gossip is multi-hop mesh → its us-east→APAC delta is geographic **+** mesh overhead.

## Run it (one per observer box, same clock as the observer)
```bash
pip install -r requirements.txt   # just `websockets`
python flashblocks_recorder.py \
  --url wss://<mainnet-flashblocks-ws> \
  --region sydney \
  --out /var/lib/base-observer/flashblocks-sydney.csv
```
Use the **same** `--region` label as the observer's `--p2p.latency.region`, start it in the same
window, and keep chrony synced (see `../deploy/setup_chrony.sh`).

> Confirm the current Base mainnet flashblocks websocket URL (docs.base.org / your builder's
> `--flashblocks` endpoint). Sanity-check the first CSV rows have a `block_number`; adjust
> `extract_block_number` if the live message schema differs.

## Output
CSV: `recv_wallclock_ns,block_number,flashblock_index,region` — one row per flashblock message.

## Decompose (join to the gossip observer CSVs on `block_number`)
Take the **last** flashblock seen per `block_number` (block complete) as the flashblock arrival.

- **P2P overhead** (per region) = `gossip.recv_wallclock_ns − flashblock.recv_wallclock_ns`
- **geographic floor** (to APAC) = `flashblock.recv[apac] − flashblock.recv[us-east]`  (same block)
- sanity: total gossip spread (from the observer analysis) ≈ geographic floor + P2P overhead

Quick anecdotal alternative (no script): `websocat wss://<url> | while read l; do echo "$(date +%s.%N) $l"; done`
on a us-east and an APAC box for a few minutes, then eyeball a handful of block numbers.
