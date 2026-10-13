# BLS signature DA experiment

Research-only example. Do not use these public test keys or unaudited implementations with real funds.

There are two executable paths: a development-chain-only **native transaction type `0x7a`** in this checkout, and the earlier Solidity control experiment. The native path changes consensus on chain **84538453 only**; it is not a mainnet activation proposal. `estimate.mjs` remains a separate counterfactual sizing experiment for general mainnet traffic.

## Native transaction flow

`native.mjs` exercises raw RPC submission, txpool admission, block inclusion, native receipts, and real EOA balance/nonce changes. It funds 32 owners and 32 existing recipients with ordinary ECDSA transfers, registers each owner's BLS key with a PoP, and sends 1,000 transfers in 33 native batches (one transfer, 31 batches of 32, then seven). No vault contract holds the transfer balances.

The outer transaction has ordinary ECDSA authorization and EIP-1559 fee fields. For registration it authorizes the existing owner to install its immutable BLS key; for a transfer batch it identifies a separate relayer paying execution and DA fees. A batch carries one 48-byte aggregate, not one signature per transfer. Inner records bind the full sender address, account nonce, recipient, amount, chain ID, and native application domain. This prototype supports only transfers between code-empty accounts, not arbitrary EVM calls, key rotation, or general account abstraction.

Registration input contains the **96-byte compressed G2 key plus its 48-byte G1 PoP**. Both enter L1 batch data. The native registry at `0x43000000000000000000000000000000000000b1` stores the key in three storage words; later transfers refer to the owner address and do not republish its key or proof. This address is outside the `0x4200…` range that genesis fills with proxy code; no genesis modification or preloaded keys are needed. The owner address is not free: each transfer's 60-byte record contains two full 20-byte addresses, an 8-byte nonce, and a 12-byte amount. Counts are bounded at 32 and senders must be strictly increasing.

`verify-native.mjs` checks a fresh node with no L2 peers or producer connection, deriving safe blocks from the real Rust batcher's L1 blobs. It compares block hash and all three roots, exact native transaction bytes/hashes, receipts, balances, and nonces. It extracts registration keys and proofs from the derived transactions and independently verifies every PoP and aggregate using noble; the node uses arkworks. Expected JSON is a checker input, never imported into the node.

### Run the native devnet

Use a fresh disposable data directory. Do not run alongside another repository devnet: container names and ports are shared. Build the setup image as in the contract instructions below, then run from the repository root:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=8 \
  RUSTFLAGS='-C link-arg=-fuse-ld=lld' cargo build --locked -p base --bin base \
  --config 'profile.dev.package.ark-ff.opt-level=3' \
  --config 'profile.dev.package.ark-ec.opt-level=3' \
  --config 'profile.dev.package.ark-bls12-381.opt-level=3' \
  --config 'profile.dev.package.ark-serialize.opt-level=3' \
  --config 'profile.dev.package.sha2.opt-level=3' \
  --config 'profile.dev.package.base-common-evm.opt-level=3'
export NATIVE_BASE_BINARY="$PWD/target/debug/base"
export DEVNET_ROOT=$(mktemp -d /tmp/base-native-bls.XXXXXX)
dc() {
  sudo --preserve-env=DEVNET_ROOT,NATIVE_BASE_BINARY docker compose \
    --env-file etc/docker/devnet-env -p bls-da \
    -f etc/docker/docker-compose.yml \
    -f etc/scripts/devnet/bls-da/compose.yaml \
    -f etc/scripts/devnet/bls-da/native-compose.yaml "$@"
}
dc up -d --no-build setup-devnet l1-el l1-cl l1-vc base-builder base-batcher
npm ci --prefix etc/scripts/devnet/bls-da
node etc/scripts/devnet/bls-da/native.mjs /tmp/native-demo.json

# The replay node must start with no L2 state.
sudo test ! -e "$DEVNET_ROOT/l2/base-rpc"
dc up -d --no-build --wait base-rpc
sudo docker network inspect bls-da_l1-only \
  --format '{{.Internal}} {{range .Containers}}{{.Name}} {{end}}'
# Expect: true, and only base-rpc, l1-el, l1-cl.
IP=$(sudo docker inspect base-rpc \
  -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
VALIDATOR_RPC_URL=http://$IP:8645 VALIDATOR_CL_RPC_URL=http://$IP:8649 \
  node etc/scripts/devnet/bls-da/verify-native.mjs /tmp/native-demo.json /tmp/native-replay.json
dc down
```

The crypto profile overrides matter: an unoptimized 32-message pairing can exceed the devnet's block-building time budget. A release build is another option. The overlay mounts the rebuilt binary in every Base service, including `base-batcher`. Do not substitute the external Go batcher: its transaction decoder does not know `0x7a`. The base container image supplies the entrypoint and userspace, not the Base executable being tested. The demo records the binary SHA-256 and client version.

### Native security and fee boundaries

The PoP ciphersuite requires a verified, domain-separated PoP before a key can authorize transfers. Execution also checks canonical encoding, curve/subgroup membership, non-identity points, ownership authorization, account nonces, balances, and the development-chain gate. Checking point encodings in the consensus codec alone is insufficient. The same execution checks must run on direct block/Engine inputs, not just at RPC admission. Invalid authorization must not leave fee, balance, nonce, or registry changes behind.

Native batches charge **exactly 21,000 execution gas per transfer**, including batch overhead and payload processing: 21,000 for one, 672,000 for 32, and 21,000,000 for 1,000. There is no additional execution base or calldata charge for batches. Registration retains its separate verification/storage/input-byte schedule; L1 data and operator fees remain separate. The demo submits batches with exactly this gas limit and checks receipt gas used, including the one-transfer boundary below ordinary EVM calldata intrinsic gas. Reth's estimator may return a slightly higher gas limit; unused gas is not charged.

One aggregate does **not** imply one-message verification: independent transfer messages still require one pairing term per message, plus the aggregate term. The experimental gas price is not a measurement of execution or proving cost. No new per-block BLS verification limit is introduced; benchmark validator/prover capacity and admission load before production pricing. Passing devnet tests is not a formal cryptographic proof or an independent audit; the scheme assumptions and alternative rogue-key defenses are discussed below.

The native demo reports normalized Brotli-compressed singular channels for mined native envelopes and signed direct-EOA references, including registration envelopes, keys, PoPs, and migration signatures in the cold case. It retains batch boundaries and also reports a per-batch-flush sensitivity. Its repeated 32 sender/recipient pairs and small amounts are unusually compressible. These estimates exclude live blob padding and are not a Base-wide throughput forecast. The direct references are sized, not broadcast.

There is **no fixed 1/2.7 fee multiplier**. Only a measured DA component can fall with compressed bytes; execution, proving, operator fees, registration, and batching overhead remain separate costs. A DA-limited capacity multiplier is the baseline compressed-byte count divided by the alternative count at the same blob budget and fill ratio. It is not automatically the multiplier for total TPS or total fees.

### Native devnet results at 21,000 gas per transfer

A fresh run registered 32 keys and executed **1,000 transfers in 33 batches**, consuming **21,000,000 execution gas**. The first batch contained one transfer: the signed gas limit and receipt gas used were both 21,000 (`0x5208`), while `eth_estimateGas` returned 21,165. Every 32-transfer batch used exactly 672,000, and the final seven-transfer batch used 147,000. All 18 live checks passed, including 15 denied authorization/validation cases without fee or nonce changes. The EVM and txpool suites passed 527 tests; targeted Clippy and nightly formatting checks passed.

A fresh L1-isolated node reproduced checkpoint **1026** (`0x402`), block hash `0xae771207d1442f1801ba33f8700bfa0f8cb64e0f21cadd16f10fb4263dcadff5`, all three roots, and **129 receipts**. Independent noble verification recovered 32 registration keys/PoPs and verified 33 aggregates covering all 1,000 transfers from derived transactions. The node had no L2 peers, sequencing was disabled, and its only network neighbors were the L1 execution and beacon nodes. The L1 scan found three successful blob transactions.

For this repeated-address workload, the normalized one-channel model measured 69,875 direct-EOA bytes versus 8,150 native bytes (8.57× warm DA capacity); including registration measured 16,187 native bytes. Flushing each batch separately instead measured 95,218 versus 52,337 bytes (1.82×). These are modeled compressed-channel bytes, not observed blob utilization or production TPS. The current gas rule changes the charged gas, not the amount of cryptographic work.

### Native devnet results with the original gas schedule

The following measurements precede the 21,000-per-transfer price. The original 101.85M-gas result reflects that earlier pricing rule, not measured CPU work. Its transaction hashes and roots belong to that historical devnet; a fresh devnet is required for the current rule.

The rebuilt checkout executed **32 PoP registrations and 1,000 transfers in 32 native batches** (31 × 32, then 8), using 1,536 aggregate-signature bytes in total. The unchanged checkout rejected the identical raw registration with `failed to decode signed transaction`; the rebuilt binary included it successfully. An initial integration run also correctly rejected the proposed registry address because it contained genesis proxy code; moving outside the predeploy range fixed that without altering genesis or weakening validation.

The final run passed 18 live checks, including 15 denied cases (forged/rogue/missing/wrong-domain PoPs, malformed points, altered or missing authorization, wrong chain, unregistered/duplicate senders, insufficient balance, contract recipients, key replacement, and replay). Every denied transaction left the outer balance and nonce unchanged. The separate ordinary-ECDSA funding transfers still executed normally. `eth_estimateGas` covered every successful native registration and batch, and RPC fields reproduced the exact raw bytes and hashes.

A new, isolated node derived checkpoint **3062** (`0xbf6`) from L1 blobs and matched its block hash, state root, transaction root, and receipt root. It reproduced all **128 receipts**, 32 sender/recipient balance pairs, account nonces, 32 PoPs, and 32 aggregates / 1,000 transfers. It had zero execution peers and sequencing disabled; its only network neighbors were the L1 execution and beacon nodes. The checker independently verified the derived keys/proofs/aggregates with noble while the node used arkworks. The L1 scan found nine successful blob transactions. Deliberately changing the expected state root made the checker fail. These heights and counts identify this run, not fixed protocol constants.

For these **same 32 sender/recipient pairs**, normalized Brotli-9 sizing gave:

| 1,000-transfer comparison | Direct EOA bytes | Native BLS bytes | DA-capacity ratio |
| --- | ---: | ---: | ---: |
| Warm keys, one channel retaining 32 block boundaries | 69,754 | 7,974 | 8.75× |
| Same channel model, including all 32 native registrations | 69,754 | 15,974 | 4.37× |
| Warm keys, flush each batch separately | 95,036 | 51,991 | 1.83× |

The registration-only channel measured 8,020 bytes (250.63/account), including compressed keys, PoPs, outer migration signatures and envelopes. Common funding transfers are excluded from both sides. Actual native transfer execution used **101,845,220 gas**, versus 21,000,000 for 1,000 ordinary EOA transfers: **4.85× more execution gas**. Direct references were signed for sizing, not broadcast. The unusually compressible repeated addresses and the large difference between flush policies rule out treating 8.75× as a Base-wide forecast or observed live-blob utilization gain. The nine live blobs prove DA transport and reconstruction; the table is a separate controlled byte model.

Combined checks passed 1,211 unit/integration tests and three doctests across consensus, protocol, EVM, RPC types, txpool, RPC, execution EVM, and flashblocks; targeted Clippy with warnings denied and nightly formatting checks passed. The separate experimental `evm2` rejects `0x7a`; the running node and proof executor use `base-common-evm`. Bare-metal `no_std` compilation checks that the native verifier remains available in the proof path, but neither proof generation nor production proving cost has been measured.

## Earlier Solidity control experiment

`demo.mjs` deploys `BLSDAExample.sol` on the Base devnet and registers 32 funded accounts with **on-chain proof-of-possession verification**. It executes ECDSA/BLS control bundles of 1, 8, and 32 transfers, then **1,000 BLS transfers to existing recipients** in 32 batches (31 of size 32, one of size 8). Every BLS batch contains one 48-byte aggregate signature. Accounts sign their individual transfer intents, not a shared batch message; the PoP scheme also supports this distinct-message aggregate verification.

Both authorization modes share the same packed records and state machine. The BLS mode uses EIP-2537 precompiles and RFC 9380 hash-to-curve. Registration publishes the public key and its PoP once, before any spending; later batches contain neither individual signatures nor repeated public keys/PoPs. The outer relayer transaction still has its ordinary ECDSA signature. This is an EVM batch-account prototype, not a native transaction type or fee discount.

`verify-da.mjs` then checks a fresh node deriving **safe** blocks from L1 only. The compose overlay puts that node on an internal network shared only with the L1 execution and beacon nodes. It has zero execution peers, no consensus discovery, and no producer connection. Base v1.4.2's normal `rpc` mode waits for an initial gossip payload; `sequencer --sequencer.stopped` instead bootstraps from genesis without producing blocks. Its CLI-required dummy scalar is not the producer's key. The verifier asserts sequencing is inactive.

The expected-output JSON is read by the verification script, never imported into the replay node. Genesis and protocol configuration are shared; no L2 state, payloads, registrations, or signatures are copied to it. The replay checks exact state/transaction/receipt roots, every account and recipient balance, registration calldata including each PoP, one signature per workload batch, and receipts including rejected registrations and transfers.

## Run the Solidity control devnet

Requirements: Linux, Docker Engine with Compose supporting `!override`, Buildx, and Node.js 20.19 or newer. The orb's `.agents/setup` installs missing Docker plugins. Run from the repository root. These commands use the repository's fixed devnet container names: **do not run alongside an existing devnet or reuse an existing replay database**. The temporary `DEVNET_ROOT` below keeps existing `.devnet` data untouched.

```sh
sudo docker build -f etc/docker/Dockerfile.devnet -t devnet-setup:local-v2 .
sudo docker build -f etc/docker/Dockerfile.op-batcher -t op-batcher:local .

export DEVNET_ROOT=$(mktemp -d /tmp/base-bls-pop.XXXXXX)
dc() {
  sudo --preserve-env=DEVNET_ROOT docker compose --env-file etc/docker/devnet-env -p bls-da \
    -f etc/docker/docker-compose.yml \
    -f etc/scripts/devnet/bls-da/compose.yaml "$@"
}
dc up -d --no-build setup-devnet l1-el l1-cl l1-vc base-builder op-batcher
dc ps

npm ci --prefix etc/scripts/devnet/bls-da
node etc/scripts/devnet/bls-da/demo.mjs /tmp/bls-demo.json

# Start only after the demo, with an empty replay datadir.
dc up -d --no-build base-rpc
sudo docker network inspect bls-da_l1-only \
  --format '{{.Internal}} {{range .Containers}}{{.Name}} {{end}}'
# Expect: true, and only base-rpc, l1-el, l1-cl (order may vary).
IP=$(sudo docker inspect base-rpc \
  -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
VALIDATOR_RPC_URL=http://$IP:8645 VALIDATOR_CL_RPC_URL=http://$IP:8649 \
  node etc/scripts/devnet/bls-da/verify-da.mjs /tmp/bls-demo.json /tmp/bls-da-proof.json

# Read-only mainnet sampling, not sending transactions:
SAMPLE_END_BLOCK=52539836 \
  node etc/scripts/devnet/bls-da/estimate.mjs /tmp/bls-estimate.json

dc down
```

Wait for the producer's RPC to become healthy before `demo.mjs`; the verifier waits up to ten minutes for the required safe head. The internal replay network intentionally may not support Docker's published localhost ports; the Linux bridge IP above works from the host. Never enable sequencing on the replay node. `dc down` stops the stack but retains its disposable bind-mount data under `DEVNET_ROOT` for inspection.

The tested stack used published `ghcr.io/base/node:v1.4.2`, not a build of the checkout's current Rust sources. L1 used Reth v2.5.2 and Lighthouse v8.2.2. The repository's setup and op-batcher images were built locally. The demo requires working EIP-2537 precompiles and refuses non-loopback RPCs and non-devnet chain IDs.

## Solidity control: measured execution and modeled DA

A successful PoP run produced **30 passing checks**, including honest registration, missing/wrong/rogue proofs, mined transfers, replay rejection, wrong chain/contract domains, altered amounts, missing signers, malformed points, and mined invalid registration/transfer rollback. An isolated replay reproduced block **640**'s hash, state root, transaction root, and receipt root, all **32 accounts and registration proofs**, and **73 receipts**. It decoded all **32 workload batches / 1,000 transfers** from its derived chain. The L1 scan observed **9 successful blob transactions** at verification time. These numbers identify one run, not fixed expected block heights.

For the 32-transfer round:

| Quantity | ECDSA bundle | BLS bundle |
| --- | ---: | ---: |
| Inner signature bytes | 2,080 | 48 |
| Normalized compressed channel bytes | 3,151 | 1,117 |
| Same-state gas estimate | 881,130 | 3,539,485 |

Ordinary direct EOA transfers measured 3,036 compressed bytes: the warm BLS transfer bundle saves **63.2%**, giving **2.72×** modeled DA-limited transfer capacity. This is a specialized transfer workload, not a Base-wide TPS claim. One-transfer bundling is actually larger than a direct EOA transfer (320 versus 200 bytes).

The demo really publishes its registry: uncompressed EIP-2537 G2 keys are 256 bytes each, with 192 coordinate bytes and 64 padding bytes before compression, plus a 48-byte PoP. Registration transactions, proofs, funding, and ECDSA control owners together measured **344.66 compressed bytes/account**. Adding that cost breaks even after about **6 transfers/key**, reaches **1.37×** capacity at 10 uses and **2.47×** at 100 uses. Those amortization figures exclude the shared deployment; the JSON separately includes deployment and all registration costs (15,544 compressed bytes total).

The actual 1,000-transfer workload consumed **109,740,329 execution gas**, used **1,536 aggregate-signature bytes** (32 × 48), and registered no additional keys. A synthetic channel packing all of its mined envelopes compressed to 6,488 bytes versus 68,720 for 1,000 signed direct-EOA references. This unusually compressible fixture repeats the same 32 recipients and small amounts: it is not 1,000 distinct recipients, a production savings forecast, or actual multi-block channel accounting. The direct-EOA references were sized, not broadcast; the BLS transfers were all mined and independently derived.

Sizes use normalized SingleBatch envelopes and Brotli quality 9, including transaction envelopes and the relayer signature. They are **not per-bundle attribution from live L1 blobs**. The actual devnet proves execution and blob-only reconstruction; the size model isolates authorization and avoids attributing channel padding, unrelated traffic, or flush timing to a signing scheme. The roughly 4× gas estimate versus the ECDSA bundle also means a DA gain need not become an execution-throughput gain.

## Native-format estimate includes keys and account identity

`estimate.mjs` reconstructs and hash-checks every non-deposit transaction in eight mainnet blocks. It preserves calldata, access lists, and nested EIP-7702 authorization signatures. It replaces only outer ECDSA authorization with one aggregate per block, retains a full **20-byte sender address per transaction**, and charges registrations for every unique sender in cold cases. Keys and aggregates are deterministic entropy placeholders, not usable signatures. This is a wire-size experiment, not an executable protocol fork.

The sample ending at block 52,539,836 contained 3,776 transactions and 1,739 senders; baseline compressed size was 146.33 bytes/transaction:

| Native BLS choice | Prior L1 registration | New keys, no migration auth | New keys + ECDSA migration auth |
| --- | ---: | ---: | ---: |
| 48-byte aggregate / 96-byte key | 38.19% saving | 6.07% saving | **14.12% larger** |
| 96-byte aggregate / 48-byte key | 38.12% saving | 21.87% saving | 0.92% saving |

Migration adds a 65-byte ECDSA authorization per account, binding an existing identity to a BLS key. Registration addresses and keys are included. Separate transaction envelopes and gas fields are not: this assumes batched native registrations, so cold costs are optimistic. Adding a PoP per key makes both migration cases about 30% larger than baseline in this sample.

A second eight-block sample ending at 52,538,036 contained 1,452 transactions and 803 senders. It gave only 21.15–21.24% warm savings; even the small-key migration case was 3.98% larger than baseline. Two short samples are not a workload forecast. They suggest **1.27–1.62×** warm DA-limited capacity, with initial migration potentially losing rather than gaining capacity.

Investigate **96-byte aggregates with 48-byte keys** for a native protocol: the extra 48 bytes per aggregate is usually cheaper than an extra 48 bytes per newly registered key. A useful raw-byte approximation is:

```text
cost/tx = unsigned payload + sender reference + aggregate_bytes/bundle_size
          + (key + registration metadata + migration authorization + optional PoP)/uses_per_key
```

Compression changes every term. Existing Ethereum transaction sender identities are recovered from ECDSA; removing signatures does not make sender information free. A fixed registry can amortize keys and use IDs, but its complete creation/update history must be in L1 DA. Publishing a key hash without the key is insufficient.

For comparison, compact ECDSA (EIP-2098) packs the recovery bit into 64 bytes but does not aggregate; typed transaction parity is already small/compressible. Schnorr signatures are also roughly 64 bytes; multisignature protocols for a shared message are not a drop-in, noninteractive aggregate of arbitrary independent transactions. BLS offers the demonstrated aggregation property but changes keys, verification, and the transaction format.

## Translate bytes into a blob-budget throughput bound

Under the [BPO2 parameters](https://eips.ethereum.org/EIPS/eip-8135), the L1 target is 14 blobs per 12-second slot, not the 21-blob burst maximum. With 130,044 usable bytes/blob and 24 bytes reserved for a version and one frame header, the modeled budget is **151,690 bytes/second across all L1 users**.

```text
Base DA-limited TPS = 151,690 × Base_share × fill_ratio / compressed_bytes_per_tx
```

For the larger sample, the full-budget upper bound is 1,037 TPS before, versus 1,675 TPS after warm small-key BLS registration. Multiply both by Base's actual share and fill ratio. This does not measure Base's current allocation or prove that current throughput is DA-bound. The blob target is an economic fee target, not a hard block cap; sustained usage above it raises fees. Execution, networking, and proof generation can impose lower limits.

## Security argument and proof of possession

The demo uses the [CFRG BLS proof-of-possession scheme](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-bls-signature-07#section-3.3). `register` validates a non-identity subgroup public key, converts its EIP-2537 representation to the canonical 96-byte compressed key (`x.c1 || x.c0`, sort bit based on `y.c1` then `y.c0`), hashes those bytes in the **PoP domain**, and checks the proof pairing before admitting/funding the account. The immutable registry has no alternate admission path, so membership implies successful PoP verification. Proof bytes remain in L1 history, not contract storage.

Transfer signatures use the separate `BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_` domain; PoPs use `BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_`. Every signed record binds the account ID, nonce, recipient, amount, chain ID, contract, and application domain. Strictly increasing account IDs keep batch encoding deterministic and restrict each account to one transfer per batch. Keys are immutable. Signatures are canonically decoded and subgroup-checked; invalid bundles atomically revert. Fees are paid by the relayer's ordinary transaction; this vault does not expose arbitrary user calls or gas-payment authorization.

For signatures in G1 and keys in G2, verification checks the pairing product of each hash-to-curve/message with its key against the aggregate signature paired with the generator. Published security analyses and their computational/random-oracle assumptions are listed in the draft's Appendix C. The implementation additionally relies on Keccak collision resistance, SHA-256/RFC 9380 hashing, correct EIP-2537 precompiles, secure production key generation, and correct account authorization rules. Successful tests are not a formal proof or an audit of this Solidity implementation.

- **Basic, with enforced distinct messages:** no PoP required for aggregate unforgeability. The aggregate still needs roughly one pairing term per message; a single stored signature does not make verification constant-cost.
- **Message augmentation:** signing `public_key || message` supplies a different rogue-key defense without a separate PoP. It does not turn distinct transfers into same-message fast verification.
- **PoP scheme / same-message fast aggregation:** validate a domain-separated PoP for every key before use. Subgroup validation is not a PoP. Publish each PoP with its registration in L1 DA; replay nodes verify it and can store a verified flag rather than retaining proof bytes in execution state.

`demo.mjs` reproduces the classic rogue-key attack against a naive same-message verifier: choose `PK_rogue = aG - PK_honest`, then `aH(message)` verifies against their summed key without the honest signer participating. It submits that rogue key and a forged PoP to the real contract: registration must revert without creating an account or retaining the attached deposit. Missing proofs, another key's proof, transaction-domain signatures used as proofs, and non-subgroup proofs are rejected too. Honest proofs are accepted, with both G2 compressed-key sign-bit branches exercised. A mined invalid transfer batch separately demonstrates atomic balance/nonce rollback. The fresh L1-only node reproduces both failed transactions as well as successful operations.

A PoP establishes possession of the **new BLS secret**, not authority over an existing ECDSA account. Migration separately needs authorization from the existing owner. Likewise, signing one whole block with a sequencer's key proves only what that sequencer signed; it cannot replace users' authorization. A validity proof could attest to omitted ECDSA verification, but needs its own soundness and reconstructability design and is outside this demo.

## Production gaps beyond the native transfer prototype

The contract demo preserves existing Base derivation because registrations, packed actions, and aggregate signatures are ordinary L2 transaction calldata carried in L1 blobs. No sequencer-only public-key database is needed. L1-only reconstruction means genesis/configuration plus **historical L1 data**, including archived blob sidecars after normal retention expires; hashes alone do not recover expired blobs.

A native format cannot simply delete ECDSA signatures from existing batches: original ECDSA signatures and transaction hashes cannot be recovered from BLS aggregates. The native transfer prototype above supplies a new envelope and hash, derivation, execution, and receipt/RPC handling for its limited transaction contract. Production still needs wallet integration, key rotation/recovery, a fork specification, independent security review, and demonstrated fault/validity-proof parity and cost. Messages must bind all authorized fields and key versions when extending beyond the transfer subset. Benchmark verification and proving before promising a DA multiplier as actual Base TPS.
