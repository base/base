# base-proof-tee-registrar

Automated TEE prover signer registration service.

Discovers new TEE prover instances via AWS ALB target group, generates ZK proofs
of their Nitro attestation certificates via the Boundless Network, and registers
their signers on-chain via `SystemConfigGlobal`.
