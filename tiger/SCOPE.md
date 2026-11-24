# Tiger client scope

This document says what the tiger client, a Base execution client written without reth or revm,
does and does not do. The rule: tiger implements what a Base operator's follower or RPC node
(`base-reth-node` driven by `base-consensus`) actually runs, with the same results, and leaves
the rest out. "Same results" means the same blocks accepted and rejected, the same state roots,
receipts and headers, and the same JSON-RPC answers for the methods listed below.

The specification is the base/base code: `crates/common/evm2` and `crates/common/evm` for
execution, `crates/execution/consensus` for header and block rules, `crates/execution/payload`
for block building, `crates/execution/rpc` and reth for JSON-RPC, and `crates/consensus/engine`
for how the consensus node drives the execution layer.

## In scope

### Block execution, every Base fork from genesis

- Chains: Base mainnet (8453) and Base Sepolia (84532), plus a custom genesis file (devnets and
  the state fuzzer's synthetic chain).
- Forks Bedrock through Denim, decided by block number and timestamp exactly as on mainnet.
  Everest is not scheduled on any chain and is out (see below).
- EVM: evm2 with the Base spec mapping (`MERGE` .. `OSAKA`).
- Transactions: legacy, EIP-2930, EIP-1559, EIP-7702 (from Isthmus), deposits (`0x7e`, Regolith
  rules: mint, nonce, failed deposits, system-tx rejection). Blob transactions are rejected.
- Fees: L1 data fee (Bedrock, Ecotone, Fjord FastLZ), operator fee (Isthmus, Jovian scaling),
  fee vault distribution, Azul transaction gas limit cap.
- Block rules: deposit gas accounting, Jovian DA footprint (`blobGasUsed`), Isthmus
  withdrawals root (the `L2ToL1MessagePasser` storage root), Isthmus empty requests hash.
- System calls: EIP-4788 beacon roots (Ecotone), EIP-2935 history storage (Isthmus).
- Upgrade transitions done by the execution layer: the Canyon create2 deployer, the Denim
  `BaseTime` predeploy. (Ecotone/Fjord/Isthmus/Jovian upgrade transactions are deposits the
  consensus node puts in the block; they need nothing special.)
- Precompiles: the Ethereum set for each spec, RIP-7212 P256 (Fjord, repriced at Azul), the
  Granite/Isthmus/Jovian input caps, and the Base native precompiles: activation registry,
  B20 asset, stablecoin, factory and policy (Beryl, v2 at Cobalt, v3 at Denim), transaction
  context and nonce manager (Cobalt).

### Block validation and import (Engine API)

- Header checks against the parent from `crates/execution/consensus`: timestamps (Denim
  subsecond schedule), EIP-1559 base fee with Holocene/Jovian extra-data parameters and minimum
  base fee, extra data format, blob fields, requests hash, withdrawals, ommers, nonce.
- Post-execution checks: gas used, receipts root, bloom, state root, DA footprint, withdrawals
  root.
- Engine API on the auth port with JWT (HS256, ±60 s `iat`), over HTTP:
  `engine_newPayloadV2/V3/V4`, `engine_forkchoiceUpdatedV1/V2/V3` (V3 with null attributes at any
  timestamp), `engine_getPayloadV2/V3/V4/V5`, `engine_exchangeCapabilities`,
  `engine_getClientVersionV1`, `engine_getPayloadBodiesByHashV1/ByRangeV1`. Statuses as the
  consensus node expects them: `VALID`, `INVALID` with `latestValidHash`, `SYNCING` for an
  unknown parent, `ACCEPTED` for a valid side block, `-38002` for an invalid forkchoice state.
- The `eth_*` reads the consensus node makes on the auth port: `eth_getBlockByNumber`,
  `eth_getBlockByHash` (full transactions), `eth_getProof`, `eth_chainId`, `eth_blockNumber`.
- Payload building from attributes with `noTxPool` (the consensus node's consolidation fallback,
  and devnet sequencing): attribute transactions only, `gasLimit`, `eip1559Params`,
  `minBaseFee`, extra data encoding, payload id exactly as base-reth computes it, build once.

### State, persistence, reorgs

- Persistent storage written by tiger itself: an on-disk copy-on-write B+tree (one file, atomic
  commits, crash safe) with tables for headers, bodies, receipts, transaction lookup, flat
  account/storage/code state, trie nodes, and per-block change sets.
- Merkle Patricia trie with stored nodes, updated incrementally per block; the same trie code
  computes roots over a stateless witness (the adapter) and over the database (the node).
- Side blocks validated against any known parent; forkchoice to a different branch unwinds the
  persisted chain with change sets and applies the new branch; restarts reopen the same state.
- History: state at any past block for `eth_call`, `eth_getBalance` and friends, and
  `eth_getProof` at past blocks (base-reth's default is an archive node; pruning is out).

### JSON-RPC (HTTP on the RPC port)

The methods the RPC and state fuzzers compare, with base-reth's answers:

- `eth_`: `chainId`, `blockNumber`, `syncing`, `gasPrice`, `maxPriorityFeePerGas`,
  `blobBaseFee`, `feeHistory`, `getBlockByNumber/ByHash`, `getBlockReceipts`,
  `getBlockTransactionCountByNumber/ByHash`, `getUncleCountByBlockNumber/ByHash`,
  `getUncleByBlock*AndIndex`, `getTransactionByHash`, `getTransactionByBlock*AndIndex`,
  `getRawTransactionByHash`, `getRawTransactionByBlock*AndIndex`, `getTransactionReceipt`,
  `getTransactionBySenderAndNonce`, `getBalance`, `getCode`, `getStorageAt`,
  `getTransactionCount`, `getProof`, `call`, `estimateGas`, `createAccessList`, `simulateV1`,
  `getLogs`, `newFilter`, `newBlockFilter`, `getFilterChanges`, `getFilterLogs`,
  `uninstallFilter`, `config`, `sendRawTransaction` (forwarded to the sequencer).
- Base fields in responses: receipt `l1Fee`/`l1GasPrice`/`l1GasUsed`/scalars/operator fee
  fields/`daFootprintGasScalar`, deposit fields, Denim `timestampMs`.
- `debug_`: `getRawHeader`, `getRawBlock`, `getRawTransaction`, `getRawReceipts`,
  `traceTransaction`, `traceCall`, `traceBlockByNumber/ByHash` with the struct logger,
  `callTracer`, `prestateTracer`, `4byteTracer`, `noopTracer`, `flatCallTracer` (evm2's
  inspectors), `executionWitness`.
- `trace_block`, `trace_transaction` (parity traces from the same inspectors).
- `net_version`, `web3_clientVersion`, `rpc_modules`.

### Differential testing

- `tiger diff-adapter`: the protocol in `etc/tools/diff-fuzz/PROTOCOL.md` (build, validate,
  systemConfig) and `TRIE_PROTOCOL.md` (trie), with optional coverage, so every fuzz mode can
  compare tiger with base-reth. Build and validate run on the node's own block execution and
  trie code; trie mode runs the node's trie over its own database.
- The node itself runs under the RPC and state fuzzers (`base-diff-fuzz rpc` / `state`).

## Left out

- **reth's devp2p networking and execution-layer sync.** Base nodes sync through the consensus
  node. A tiger node starts from genesis (or from an imported state) and is fed blocks over the
  Engine API, for example by `base-consensus` in follow mode (`--source-l2-rpc`) or as a
  sequencer/devnet. Consequence: a validator whose EL is far behind the tip cannot rely on the EL
  syncing itself (base-consensus waits for a `VALID` forkchoice); follow mode covers that.
- **The mempool.** Peer transaction gossip is off on Base, and RPC nodes forward
  `eth_sendRawTransaction` to the sequencer. Tiger forwards and keeps no pool, so `txpool_*`,
  pending-block contents from local transactions, and builder forwarding
  (`--enable-tx-forwarding`, `base_insertValidatedTransaction`) are out.
- **Sequencer-side building from a pool**, `miner_*`, flashblocks (removed from base/base).
- **Everest**: Base transactions (`0x79`), 2D nonces, EIP-8430, gas-limit billing, payer RPC.
  Not scheduled on mainnet or Sepolia; added when it is.
- **Optional base-reth extensions**: metering (`base_meter*`), proofs-history ExEx (tiger serves
  `eth_getProof` and `debug_executionWitness` from its own archive state instead), shadow
  indexer, transaction event journal, upgrade-signal contract reading, validity
  (`base_sendRawTransactionValidity`), `base_transactionStatus` (proxied to the sequencer in
  base-reth; can be added as a proxy).
- **Pruning**, WebSocket and IPC transports, `eth_subscribe`, `admin_*`, JS tracers, metrics.
- **Ethereum L1 behavior** that Base never activates (blob transactions, PoW, pre-merge rules).

## Dependencies

evm2 (with evm2-inspectors for tracing), alloy types, RLP and trie primitives, serde/serde_json
for JSON-RPC, and the engine-neutral base crates whose trees hold no reth or revm
(`base-common-consensus`, `-genesis`, `-l1-fees`, `-flz`, `-base-tx`, `-rpc-types`,
`-rpc-types-engine`). No async runtime, HTTP framework, database engine, or CLI framework:
tiger writes its own small HTTP server, JWT check, storage engine and argument parsing.
README.md lists every dependency with its reason.
