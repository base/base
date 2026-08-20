# base-proof-tee-registrar

Automated TEE prover signer registration service.

Discovers TEE prover instances via AWS ALB target groups, validates their Nitro
attestations, generates P-384 inverse hints, and submits certificate-cache and
signer-registration transactions onchain.

## Discovery Cache TTL

When an instance disappears from otherwise successful AWS/ALB discovery output
or is reported unhealthy, the registrar preserves its last-known active signers
for `--instance-cache-ttl-cycles` cycles
(`BASE_REGISTRAR_INSTANCE_CACHE_TTL_CYCLES`, default `5`). Shorter TTLs can
speed up cleanup for genuinely removed instances but increase exposure to
transient AWS/ALB flakes; longer TTLs protect against flakes but delay real
cleanup.
