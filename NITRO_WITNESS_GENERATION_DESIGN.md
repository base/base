# Optimal Nitro Witness Generation

## Constraint

Keep the existing enclave contract: build `Vec<(PreimageKey, Vec<u8>)>` on the host, then call `Prove(preimages)`. The host should construct a safe superset directly instead of running the complete fault-proof program to discover reads.

## Plan the bounded ranges

1. Fetch the agreed L2 block and decode its L1 origin.
2. The execution range is `agreed_l2_number + 1..=claimed_l2_number`.
3. The derivation range starts at `safe_l1_origin - channel_timeout` (bounded by L1 genesis) and ends at the hash-pinned L1 head. Fetch by number in parallel, then verify every parent link and the terminal head hash.
4. Include the L2 header lookback required by `BLOCKHASH`/history lookups, not only the execution range.

## Fetch in parallel

Launch these independent work groups together:

- **L2 execution:** fetch every full canonical L2 block, reconstruct `BasePayloadAttributes`, then call `debug_executePayload(parent_hash, attributes)` for every block concurrently. Merge all returned state nodes, codes, and key preimages. Calls are independent because the node reads each historical parent state. Use bounded, measured concurrency; the current RPC server permits three calls per node, so use several proof nodes or tune that semaphore.
- **L2 support:** encode required raw headers and transaction tries, and fetch the starting-output header plus `L2ToL1MessagePasser` proof.
- **L1 derivation:** fetch raw headers, full transactions, and raw receipts for every block in the derivation range. Build transaction/receipt trie preimages with the existing trie helper. After identifying blob transactions, fetch their sidecars and produce the commitment, field-element, and KZG/precompile preimages.
- **Boot data:** insert every request/config local preimage currently supplied by `BootKeyValueStore`.

## Assemble

Have workers send chunks through a bounded channel to one assembler owning `HashMap<PreimageKey, Vec<u8>>`. Validate content-addressed keys on insertion, reject conflicting values, and deduplicate across blocks. Back the map with a persistent CAS shared across jobs so overlapping proofs become cache reads. When all groups finish and ancestry/range checks pass, convert the map directly into the existing `Vec<preimages>` and send it unchanged to the enclave.

During rollout, run the current host replay only as a completeness audit and count cache misses. Remove it from the critical path after representative workloads produce zero misses. The final critical path is parallel RPC generation plus one enclave replay, with no enclave changes.
