# `base-consensus-l1`

A minimal, stateless Ethereum L1 node that runs inside the `base` binary so operators do not
need a separate L1 execution client.

## Overview

The embedded L1 keeps no L1 state. It serves the subset of the L1 execution JSON-RPC API that
the Base consensus node reads, built from data it verifies itself:

- **Trusted heads** come from an Ethereum beacon light client. It starts from a trusted
  checkpoint block root and follows the sync committee, verifying every update with BLS
  signatures and SSZ Merkle proofs. Light-client data is fetched from beacon-chain libp2p peers
  found with discovery v5, and nothing a peer sends is trusted for anything it cannot prove.
- **Headers, bodies and receipts** are fetched from Ethereum devp2p peers. Headers are accepted
  only when they hash-link back to a light-client-verified block, and bodies and receipts only
  when they match the transaction, withdrawal and receipt roots of those headers.

The served methods are `eth_chainId`, `eth_blockNumber`, `eth_getBlockByNumber`,
`eth_getBlockByHash` and `eth_getLogs` (`blockHash` filters only). The
`latest` tag is the light client's optimistic head; `safe` and `finalized` are its finalized
head. From Gloas the light client commits to the parent of each beacon block's payload, so
`latest` trails the chain tip by one block. State reads such as `eth_call` and
`eth_getStorageAt` are not available, so the unsafe block signer falls back to the genesis or
configured signer and `--upgrade-signal.contract` needs an explicit `--upgrade-signal.l1-rpc`.

Blobs come from beacon-chain peers too: the embedded L1
rebuilds each blob from data columns 0 to 63 of its block, fetched from supernodes that custody
every column. Each rebuilt blob is matched to a requested versioned hash through its recomputed
KZG commitment, which works for both the Fulu and the Gloas sidecar layouts, and the consensus
node checks it against the L1 transaction again.

Each time the verified head moves, the body, receipts and batch-inbox blobs of the 32 most
recent blocks are fetched into memory before the consensus node asks for them, so live
derivation and sequencing read L1 without waiting on peers. Older blocks, such as those read
after a restart, are still fetched from peers on demand.

Derivation, the sequencer's L1 origin selection and blob fetching read the embedded L1 in
process through `L1Provider`, with no RPC hop or JSON encoding. The L1 watcher and the
engine's startup lookup still use the local JSON-RPC server, which only carries
small or infrequent reads.

The embedded L1 also does its share of data availability: it custodies the 4 data
columns its node ID selects, as every beacon node must. For each verified L1 block with blobs it
fetches those columns from supernodes, checks the beacon header against the next block's
`parent_beacon_block_root`, the commitments' inclusion proof and every cell's KZG proof, and
serves them to peers by range and by root. Columns are kept under
`<datadir>/embedded-l1/columns` for the 18-day retention window: about 50 KB per mainnet block
today, so roughly 7 GB, and up to about 24 GB if every block carried 21 blobs. The node key
is kept in `<datadir>/embedded-l1/beacon-node.key` so the columns stay the same across restarts.

Not every peer serves Gloas light-client data: Lighthouse before v8.4 answers with an error and
Prysm has no light-client protocols. The light client stops asking a peer once it fails a request
or serves an update signed more than two epochs ago, and keeps discovering until one answers. When
no peer has a finality update the head still follows optimistic updates. The light client also
subscribes to the `light_client_finality_update` and `light_client_optimistic_update` gossip
topics and only polls while they have been quiet for two slots. Gossiped updates are verified but
never forwarded.

The light-client state is saved to `<datadir>/embedded-l1/light-client.json` on every new
finalized header and restored on restart, so only the first start needs a bootstrap. An explicit
`--l1.embedded.checkpoint` bootstraps from that checkpoint instead.

On Sepolia so few peers serve Gloas light-client data that the first bootstrap can take 10 to 15
minutes, and the head can lag for minutes at a time until a serving peer is found.

Supported L1s are mainnet and Sepolia, from Electra onwards, with no L1 URL at all. The pinned
reth chain spec predates Sepolia's Amsterdam fork, so the embedded L1 adds it to compute the fork
ID Sepolia peers expect.

## Usage

```sh
base --chain base rpc --l1.embedded --l1.embedded.port 30304 ...
base --chain sepolia rpc --l1.embedded ...
base --chain base sequencer --l1.embedded --upgrade-signal.l1-rpc <l1 rpc> ...
```

On `base sequencer`, the L1 origin selector and payload attributes read the embedded node in
process, and the L1 watcher polls its JSON-RPC server on loopback, so no L1 request leaves the
host. The verified head trails the L1 tip by about one
slot, so lower `--sequencer.l1-confs` by one to keep today's origin timing. Run the embedded node on
standby sequencers too, so a failover doesn't wait the minute or more a cold start takes to find
peers. The upgrade signal still reads its contract over `--upgrade-signal.l1-rpc`, which the
sequencer requires whenever the chain has an upgrade signal contract.

The light client starts from a checkpoint built into the release. Pass
`--l1.embedded.checkpoint 0x<trusted beacon block root>` to use your own; it must be younger than
about five months for peers to still serve its bootstrap.
