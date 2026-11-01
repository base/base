# TEE prover registrar glossary

Terms used by the TEE prover registrar, the service that discovers Nitro enclave
prover instances and keeps their signer keys registered in the onchain
`TEEProverRegistry`. Use these words in code, docs, logs, and reviews.

## Language

### Instances

**Instance observation**:
What one discovery cycle learned about one discovered prover instance:
unreachable, resolved, attestations unavailable, or failed.
_Avoid_: resolution outcome, instance result

**Unresolved instance**:
An instance whose signer set this cycle cannot be trusted as complete, because
resolution failed, attestations were unavailable, it went missing and is still
within its grace period, or it is unhealthy with no last-known signers (as after
a restart) and is still within its grace period.
_Avoid_: inconclusive instance, uncertain instance

**Grace period**:
The number of cycles an unhealthy or missing instance's last-known signers stay
protected before the registrar forgets them.
_Avoid_: cache TTL (in prose), cooldown

### Signers

**Active signer**:
A signer reported by a reachable instance this cycle, or remembered for an
instance still within its grace period.
_Avoid_: live signer, known signer

**Registerable signer**:
An active signer from a healthy instance whose attestation was fetched this
cycle, so a registration task can be started for it.
_Avoid_: eligible signer, new signer

**Orphan signer**:
A signer registered onchain that is neither an active signer nor the subject of
a pending registration task.
_Avoid_: stale signer, dead signer

**Protected signers**:
The set of signers that orphan cleanup must not deregister this cycle: active
signers plus signers with a pending registration task. Exists only for cycles
with no unresolved instance.
_Avoid_: keep set, allowlist

### Lifecycle

**Registration task**:
The background attempt to validate one signer's attestation and register it
onchain. At most one exists per signer.
_Avoid_: proof task

**Signer reconciler**:
The per-cycle decision of which registration tasks to start or cancel and
whether orphan cleanup may run, made from instance observations and pending
registration tasks.
_Avoid_: signer lifecycle manager, reconcile pass

**Orphan cleanup**:
Deregistering every orphan signer, run only when the signer reconciler yields
protected signers.
_Avoid_: orphan dereg, garbage collection
