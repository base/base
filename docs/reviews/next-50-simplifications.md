# Next 50 Base simplifications

All 50 suggestions in the second pass are implemented. Each has its own commit;
items with dependencies were applied in dependency order. This follows the first
pass, which retained JavaScript tracing at the user's request.

The workspace now has **90 packages and 612 internal dependency edges**, compared
with 94 and 642 at the start of this pass. These are structural counts, not a
build-time benchmark. See the [architecture](../architecture/README.md) and
[validation results](../architecture/validation.md).

The changes fix production execution, conversion, payload, RPC, and networking
types to Base. Database, inspector, and disk/in-memory blob-store types still vary
where both implementations are used. Ordering and snap failure fixtures remain
confined to test code. JavaScript tracing and the live authenticated Hyper
transport used by the consensus source remain available.

The dependency checker now also prohibits engine-to-RPC edges and metrics-to-chain
or RPC-schema edges. Existing state-to-RPC and singleton-directory rules remain.

| Item | Commit | Change |
|---|---|---|
| 1 | `7aa227886` | Remove tenderly-admin-api client features |
| 2 | `7bf6a2a59` | Remove erc4337-api client features |
| 3 | `d32fbf5cb` | Remove rpc-api client features |
| 4 | `f933f029c` | Remove more-tuple-impls client features |
| 5 | `aa14762df` | Remove throttle client features |
| 6 | `afba9b3bc` | Remove ws-native-tls,ws-ring client features |
| 7 | `9d4edb176` | Standardize provider HTTP on Reqwest with Rustls |
| 8 | `c012edaba` | Remove unused opcode name parser |
| 9 | `8adc4d2f7` | Remove dormant native EIP-3155 tracer |
| 10 | `1d4228e12` | Remove unused EVM backend feature forwarding |
| 11 | `e3bee8494` | Remove dormant compile-time logging caps |
| 12 | `47941ddce` | Make borrowed MDBX reads unconditional |
| 13 | `5a323e41d` | Make MDBX read transaction timeouts unconditional |
| 14 | `d1c57f804` | Make RPC serialization and std support unconditional |
| 15 | `a57f9f320` | Merge contract calls into the Ethereum client |
| 16 | `32563a9db` | Fold execution client process utilities into the Ethereum client |
| 17 | `22775ee63` | Combine proof RPC clients in one crate |
| 18 | `ff7f28363` | Combine state tasks and maintenance as state operations |
| 19 | `aa91b9e4b` | Remove engine observers dependency on the RPC server |
| 20 | `13d4decea` | Decouple channel memory accounting from chain types |
| 21 | `6752cf4ae` | Replace the production EVM factory trait with inherent Base methods |
| 22 | `08d887c4c` | Make the production EVM environment concrete for Base |
| 23 | `784b9a441` | Remove the single-implementation block environment trait |
| 24 | `c1f83fbca` | Replace transaction mutation polymorphism with Base methods |
| 25 | `154578ac7` | Replace the block executor trait with concrete Base execution |
| 26 | `396b4d50b` | Replace the outer executor trait with concrete block execution |
| 27 | `bf7937e85` | Replace the block builder trait with inherent methods |
| 28 | `afd567300` | Make block builders hold the Base executor directly |
| 29 | `7bb0dd4fa` | Store only Base transaction environments in WithTxEnv |
| 30 | `cb0c4c2d1` | Flatten Base transaction results and remove generic result traits |
| 31 | `0898d0880` | Delete unused Either EVM and executor adapters |
| 32 | `0a487d472` | Specialize transaction tracing to the Base EVM |
| 33 | `7c5096da3` | Fix trace output halt reasons to Base |
| 34 | `40f19d289` | Key precompile caches by concrete Base fork identifiers |
| 35 | `80960e8f8` | Use the canonical notification stream directly in payload services |
| 36 | `0c33ea879` | Fix transaction validators to the production blockchain provider |
| 37 | `4e8ce37e1` | Fix pool validation to the Base executor and localize ordering fixtures |
| 38 | `d5c9b707a` | Remove the provider parameter from the Base transaction pool |
| 39 | `6fe6b4367` | Fix validation task executors to the Base validator |
| 40 | `907303f89` | Share one provider and timestamp cache across RPC conversion |
| 41 | `c9d2af039` | Fix admin RPC to the Base network handle and pool |
| 42 | `37cc4e35a` | Use the blockchain provider directly for eth_config |
| 43 | `bf3a108ba` | Use the Base pool in builder RPC and verify actual insertion |
| 44 | `1f88adca2` | Fix shadow validity RPC to the Base pool |
| 45 | `c9b34b9af` | Use Base payload attributes in execution witness RPC |
| 46 | `48afde65f` | Use concrete Base request and envelope types for RPC signers |
| 47 | `636c4f2ac` | Separate network settings from the provider attached at launch |
| 48 | `3a9972539` | Fix Ethereum request serving to the production Base provider |
| 49 | `5a8e86eaf` | Connect transaction gossip directly to the Base pool |
| 50 | `9453d6b68` | Delete unused no-op transaction filter abstraction |
