# base-proof-tee-registrar

Automated TEE prover signer registration service.

Discovers new TEE prover instances via AWS ALB target group, generates ZK proofs
of their Nitro attestation certificates via the Boundless Network, and registers
their signers on-chain via `TEEProverRegistry`.

## Discovery Cache TTL

When an instance disappears from otherwise successful AWS/ALB discovery output,
the registrar preserves its last-known active signers for
`--instance-cache-ttl-cycles` cycles (`BASE_REGISTRAR_INSTANCE_CACHE_TTL_CYCLES`,
default `5`). Shorter TTLs can speed up cleanup for genuinely removed instances
but increase exposure to transient discovery flakes; longer TTLs protect against
flakes but delay real cleanup.
