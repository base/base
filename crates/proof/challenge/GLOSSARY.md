# Challenger

The challenger is the offchain service that disputes invalid proposals in ZK/TEE dispute games. This glossary fixes the words used for its proof and dispute lifecycle.

## Language

**Dispute attempt**:
One game's effort to get a proof accepted onchain, from the first prover request until the game is disputed, dropped, or ignored. It may switch proof source (TEE, then ZK fallback) without becoming a new attempt.
_Avoid_: Proof session, pending proof

**Prover session**:
One proving job on the prover service, identified by its `session_id`. A dispute attempt starts a new prover session each time it reproves or falls back.
_Avoid_: Proof session, job

**Proof source**:
The kind of proof a dispute attempt is currently getting: a TEE attestation or a ZK proof.
_Avoid_: Proof kind, proof type

**ZK fallback**:
The switch of a dispute attempt from a TEE proof source to a ZK proof source after the TEE proof fails or its dispute transaction is rejected.
_Avoid_: Retry
