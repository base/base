# Standalone User-Funded Proving

This guide shows how to run the Base prover stack on its own — no devnet — and
submit real ZK proof requests for a live network (e.g. Base Sepolia or Base
mainnet) using your own RPC endpoints and your own funded
[Succinct Prover Network](https://docs.succinct.xyz/docs/protocol/spn/architecture)
requester key.

The stack is three containers managed by `just prover up|down|logs <network>`.
The network label isolates each local control plane and its persisted jobs:

- `prover-service-postgres` — session storage (persisted under `.zk-prover/<network>/`)
- `base-prover-service` — the JSON-RPC coordinator `basectl` talks to
- `base-prover-zk-host` — the worker that generates witnesses and submits
  proof requests

## ZK backends

`basectl proofs propose` selects any proving backend with `--zk-backend`.
`basectl proofs finalize` accepts `network` or `cluster`; dry-run cannot
finalize because it produces no proof bytes to submit on chain.

| Backend | What it does | Cost | Requirements |
|---------|--------------|------|--------------|
| `dry-run` | Executes the range in the local SP1 executor and reports cycle statistics. No proof bytes are produced. | Free | The four RPC endpoints |
| `cluster` | Proves on a separately deployed SP1 GPU cluster. | Your infra | Cluster endpoint + S3 artifact bucket |
| `network` (default) | Buys the proof on the Succinct Prover Network marketplace. | Paid in PROVE | The four RPC endpoints + a funded requester key |

The standalone stack described here supports `network` and `dry-run`:
supplying all four RPC endpoints enables dry-run automatically, and the
requester key enables network. Use `dry-run` to size a range before paying for
it with `network`.

## Game range semantics

`basectl proofs finalize <GAME_OR_TX>` accepts either a dispute game proxy
address or the L1 transaction that directly created it through
`DisputeGameFactory.createWithInitData`. Basectl reads the committed block
range, L1 head, output roots, and checkpoint interval from the game instead of
accepting an arbitrary range from the operator.

## Requester key setup

Use a dedicated key for proving — do not reuse an operational or personal key.
The key is required only for the paid `network` backend; dry-run works without
it. The worker reads it as a plaintext hex private key in `NETWORK_PRIVATE_KEY`
(KMS-backed requesters are out of scope for this guide).

Follow the official
[Succinct Prover Network quickstart](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart):

1. Generate a fresh key (e.g. `cast wallet new`).
2. Acquire PROVE on Ethereum mainnet.
3. Deposit PROVE into the key's Succinct Network account via the
   [Succinct explorer](https://explorer.succinct.xyz/) account page.

Proof requests are billed against this deposited balance.

## RPC requirements

The worker generates witnesses by replaying historical state, so standard
"latest state only" RPC providers are not sufficient. All four endpoints must
serve the full history of every range you intend to prove — from the pre-state
block through the claimed block (and the L1 range that derives it). If your
provider prunes history before your target range, witness generation fails.

- **L1 execution** (`L1_NODE_ADDRESS`): historical block access, the
  `finalized` block tag, `debug_getRawHeader`, and `debug_getRawReceipts`.
- **L1 beacon** (`L1_BEACON_ADDRESS`): the genesis and spec endpoints, and
  historical blob sidecars for the L1 range that carries the batch data.
- **L2 execution** (`L2_NODE_ADDRESS`): full historical blocks,
  `eth_getProof`, `debug_getRawBlock`, `debug_getRawHeader`,
  `debug_executePayload`, and `debug_dbGet` as a fallback for preimages.
- **Base consensus** (`BASE_CONSENSUS_ADDRESS`): `optimism_rollupConfig` and
  `optimism_outputAtBlock`.

Endpoints served from your own machine must be addressed as
`http://host.docker.internal:<port>`, not `localhost` — inside the containers,
`localhost` is the container itself.

## Dev container

The checked-in dev container is the recommended packaged environment for this
workflow. It installs the repository's Rust, `just`, and SP1 toolchains plus a
private Docker daemon, so `just prover up <network>` starts the existing
Postgres, prover-service, and zk-host containers inside the dev container.

Open the repository in a Dev Containers-compatible client, locally or on a
remote development machine, then follow the operator flow below in its
terminal. `BASECTL_PROVER_RPC` defaults to `http://127.0.0.1:9000` there.
Export RPC URLs and private keys at runtime; do not add them to
`.devcontainer/devcontainer.json` or commit an environment file.

A local dev container still uses the local machine's CPU and memory for Base
witness generation. Run it on a suitably provisioned remote machine when the
local machine cannot replay the requested block range. SP1 Network proving is
remote in either case.

## Operator flow

1. Export the endpoints and, for paid proving, the requester key:

   ```bash
   export L1_NODE_ADDRESS=https://your-l1-node.example
   export L1_BEACON_ADDRESS=https://your-l1-beacon.example
   export L2_NODE_ADDRESS=https://your-base-node.example
   export BASE_CONSENSUS_ADDRESS=https://your-base-consensus.example
   export NETWORK_PRIVATE_KEY=0x...
   ```

   `NETWORK_PRIVATE_KEY` is only needed for the paid `network` backend; omit
   it to run a dry-run-only stack.

2. Build the reproducible SP1 ELFs if you have not already (paid network
   proving refuses to run against a stub-backed worker):

   ```bash
   just succinct build-elfs
   ```

3. Start a network-scoped stack. This validates the network label and the four
   RPC endpoints, checks the ELFs, builds the two images, and starts the three
   containers:

   ```bash
   just prover up sepolia
   ```

   Use the same network label in basectl (`-c sepolia`) so session IDs and the
   network-scoped local database describe the same chain.

4. Find an in-progress dispute game whose ZK proof slot is empty:

   ```bash
   basectl -c sepolia proofs games --missing-zk
   ```

   Inspect a candidate with `basectl -c sepolia proofs games <GAME_ADDRESS>`.
   Both need `proofs.dispute_game_factory` in the config (or `--factory`).

5. Optionally submit a dry-run request for that game to check RPC coverage and
   collect cycle statistics without buying a proof. Use the L1 wallet that
   would submit the real proof so the request matches the eventual flow:

   ```bash
   basectl -c sepolia proofs propose <GAME_ADDRESS> \
     --prover-address <YOUR_L1_WALLET> \
     --prover-rpc http://localhost:9000 \
     --zk-backend dry-run
   ```

   Track the printed session ID with `basectl proofs status <SESSION_ID>`.
   Dry-run results contain execution statistics but no proof bytes and cannot
   be submitted on chain.

6. Export the submitting wallet and run the paid, one-shot finalization. The
   wallet must hold enough settlement-layer ETH for the final L1 transaction;
   it is separate from the requester key whose deposited PROVE pays SP1:

   ```bash
   export BASECTL_SUBMITTER_PRIVATE_KEY=0x...

   basectl -c sepolia proofs finalize <GAME_ADDRESS_OR_CREATION_TX> \
     --prover-rpc http://localhost:9000 \
     --zk-backend network
   ```

   The confirmation prompt states the exact game, block range, payment
   backend, submitting wallet, and L1 endpoint. Finalize requests the proof,
   waits up to 24 hours, and submits `verifyProposalProof` after completion.
   Re-running the same command resumes the deterministic prover-service
   session instead of intentionally creating another paid request. If the
   earlier session failed, finalize refuses to silently purchase another proof
   and aborts; pass `--retry-failed` to explicitly retry with a new paid
   request.

Stop the stack with `just prover down sepolia`; Postgres data under
`.zk-prover/sepolia/` survives, so sessions and proof results persist across
restarts. Stream logs with `just prover logs sepolia`.

The RPC port defaults to 9000 and can be overridden with
`PROVER_SERVICE_RPC_PORT`.

## Splitting propose and submit

`proofs finalize` wraps proposal, polling, and L1 submission in one command.
When those phases need to run in separate processes, use the split commands
against the same game found in step 4 above:

```bash
basectl -c sepolia proofs propose <GAME_ADDRESS> \
  --prover-address <YOUR_L1_WALLET> \
  --prover-rpc http://localhost:9000
```

The block range, L1 head, and intermediate root interval are read from the
game on L1. The request runs both stages through your stack: a compressed
range proof, then aggregation into an ~870-byte PLONK proof; on the `network`
backend both stages are paid Succinct Network requests. The proof journal
commits to `--prover-address`, so the proof only verifies when submitted from
exactly that wallet. Track progress with `basectl proofs status <SESSION_ID>`;
the completed session stores the PLONK proof bytes.

Then submit, signing with the wallet passed as `--prover-address`:

```bash
export BASECTL_SUBMITTER_PRIVATE_KEY=0x...   # key for <YOUR_L1_WALLET>

basectl -c sepolia proofs submit <GAME_ADDRESS> \
  --prover-rpc http://localhost:9000
```

This fetches the PLONK proof from your prover service and sends
`AggregateVerifier.verifyProposalProof(proof)` to the game, waiting for the
transaction to be mined. When run with the same wallet and defaults as
`propose`, no session ID is needed — basectl derives the same deterministic
session ID both times. Pass `--session-id`, `--zk-backend`, or
`--intermediate-root-interval` if you overrode them at propose time, and
`--wait` to poll the prover service until the proof is ready before
submitting. Right before sending the transaction, basectl re-reads the game
and aborts without spending gas if it is no longer in progress or already has
a ZK proof.

Submit the ZK proof early in a game's life: a game with both a TEE proof
(from Base's proposer) and a ZK proof resolves on the fast path, and the fast
resolution window is `now + 1 day` at the moment the second proof lands, so a
late proof buys little.

## Sizing ranges

Use `proofs propose <GAME_ADDRESS> --zk-backend dry-run` to measure the exact
range committed by a candidate game before paying for it. The reported cycle
count tells you whether the range fits within the SP1 range program's limits.
There is no fixed Base-specific maximum range; it depends on the gas actually
used by the game's blocks. For a paid smoke test, choose the smallest eligible
game available rather than inventing a range that is not committed on chain.

## Restart and duplicate-submission behavior

The worker persists the Succinct Network request ID in Postgres once the
network accepts a request. After a zk-host restart, recorded request IDs resume
polling instead of resubmitting, which reduces duplicate-payment risk.

There is still a crash window after network acceptance and before the request
ID reaches Postgres. A restart in that window can submit and pay for a second
request. Keep initial ranges conservative; this workflow does not claim an
absolute no-duplicate-payment guarantee.

## Relationship to the devnet

The devnet does not run a prover. `just devnet up` brings up only the chain
stack, and this standalone stack is the only way to run the prover trio —
against a live network or a local devnet. To target a local devnet, export the
devnet's host-published RPC endpoints (addressed as
`http://host.docker.internal:<port>`, not `localhost`) and run
`just prover up devnet`. See
[etc/docker/README.md](../../etc/docker/README.md).

ZK proving requires a chain whose activation registry admin address is built
into the binary (Base Mainnet, Sepolia, and Zeronet). The stock docker devnet
uses a different chain ID, so witness generation fails against a devnet chain
unless it runs with the Beryl and Cobalt upgrades disabled.

## Troubleshooting

**`failed to run Succinct host`.** Check the zk-host worker log for the
generation failure. Prover-service may later expose a generic claim-expiry
message through `basectl proofs status`, so use the worker log for the
original error.
