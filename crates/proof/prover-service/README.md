# Base Prover Service

`base-prover-service` defines the JSON-RPC contract used to submit proof
requests, poll proof status, and coordinate worker-owned proof jobs. It also
provides the service implementation and queue-maintenance status polling.

Requester-submitted work is queued in `proof_requests` and claimed through the
worker API for both ZK and TEE proof jobs. The protocol-native service uses
`proof_requests` as the single execution queue.

Concrete proving backends live behind worker hosts. ZK hosts own SP1
cluster/network/dry-run integration, while TEE hosts own enclave
integration.

Enable `rpc-server` to generate the server trait. Use
`base-prover-service-client` for requester and worker JSON-RPC clients.

## Registry-based TEE cleanup

`prover_deleteProofsByUnregisteredTeeSigners` is a deployment-wide maintenance
operation, not a requester- or game-scoped delete. Its explicit allowlist must
be a fresh, complete registration snapshot covering every TEE proof in this
service's database. A successful empty snapshot deliberately removes every
completed TEE request with known, nonzero signer metadata.

The proposer's automatic cleanup discovers one registry through the current
`gameImpls(gameType)` implementation. Do not share its prover-service database
with requesters or existing games that use independent signer-registration
domains. Proposer and challenger requests can share the queue only when that
snapshot covers both. The RPC does not verify this deployment precondition.

Proof requests and worker claims do not carry registry or game-type routing
provenance. Their `UUIDv5` session IDs hash namespaces rather than expose a
filterable requester prefix. Supporting mixed registration domains requires
explicit request provenance and matching worker routing; filtering by a guessed
session prefix would not establish a safe deletion boundary.

The bulk operation preserves registered signers, non-TEE proofs, unfinished or
failed requests, and null/zero signer metadata. After an `InvalidSigner` result,
the proposer may additionally delete only the rejected terminal session when
the registry read fails, the bulk RPC definitely rejects the request, or the
bulk operation leaves that session protected; other registered proofs are
preserved. A failed registry read never authorizes bulk deletion. Unknown bulk
outcomes retain bounded backoff, then attempt rejected-session deletion before
recovery. Recovery is still requested if that fallback fails, because cohort
deletion may have committed. Deploy the service's bulk RPC
before enabling it in the proposer.
