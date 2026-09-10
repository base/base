# Base-only simplification

Implements suggestions 1 and 3–50. Suggestion 2 was explicitly excluded:
the optional JavaScript tracer and Boa remain in RPC, with their tests and benchmark.

The workspace now has 94 packages and 642 distinct internal dependency edges,
compared with 109 packages and 768 edges before this change. This measures
manifest structure, not compile time. See [architecture](../architecture/README.md)
and [validation](../architecture/validation.md).

| Suggestion | Result |
|---|---|
| 1 | Removed inert engine switches and their CLI arguments. |
| 2 | **Retained:** JS tracing, its feature, Boa, tests, and benchmark. |
| 3 | Removed snmalloc; retained the supported system and jemalloc allocators. |
| 4 | Removed Tracy dependencies, feature plumbing, and CLI options. |
| 5 | Removed the alternate GMP modular-exponentiation path. |
| 6 | Removed the substrate-bn alternate backend. |
| 7 | Removed chain crypto-backend injection. |
| 8 | Removed Borsh chain serialization support. |
| 9 | Removed partial trie persistence; obsolete database checkpoint tags remain recognizable and reject unsupported records. |
| 10 | Removed disable-lock; storage always acquires its lock. |
| 11 | Removed the empty node-service reth-codec feature. |
| 12 | Restricted Ethereum reference EVM constructors and context to tests/test utilities. |
| 13 | Merged RPC server assembly into RPC handlers. |
| 14 | Moved builder RPC adapters from txpool into RPC handlers. |
| 15 | Moved metering RPC adapters into RPC; computational metering remains in payload building. |
| 16 | Moved inspectors into RPC handlers, including JS tracing. |
| 17 | Merged EVM primitives, memory, machine, crypto, and precompiles into runtime; procedural macros retain their required crate boundary. |
| 18 | Moved L1 fees and compression helpers into chain configuration. |
| 19 | Merged payload builder types into common payload types. |
| 20 | Merged engine types into common payload types. |
| 21 | Merged discovery v5 implementation and discovery wrapper. |
| 22 | Merged network peer types into wire. |
| 23 | Merged provider interfaces into state types; database-specific interfaces live in database to avoid a dependency cycle. |
| 24 | Moved storage codecs into chain types. |
| 25 | Moved operational debug client logic into node service. |
| 26 | Removed EVM *For aliases and InspectorFor marker indirection. |
| 27 | Fixed BaseBlockExecutorFactory to BaseEvmFactory. |
| 28 | Fixed Base execution configuration to ChainConfig. |
| 29 | Fixed block executor EVM/configuration types; retained database and inspector parameters. |
| 30 | Removed the single-implementation BlockExecutorFactory trait. |
| 31 | Fixed proof execution builders to the Base EVM factory. |
| 32 | Production Context now varies only by database; Base transaction/configuration/chain state are fixed. |
| 33 | Fixed BaseTxResult to BaseHaltReason and OpTxType. |
| 34 | Fixed BaseEvm and BaseHandler precompile storage to PrecompilesMap. |
| 35 | Fixed inspector frame input/output types. |
| 36 | Replaced BuildNextEnv with inherent Base environment construction. |
| 37 | Fixed pending-block environment receipt type to BaseReceipt. |
| 38 | Fixed cached transaction receipt type to BaseReceipt. |
| 39 | Fixed transaction source envelopes to BaseTxEnvelope. |
| 40 | Fixed RPC transaction/receipt conversion helpers to the production provider. |
| 41 | Fixed TxPoolApi to the production Base pool. |
| 42 | Fixed txpool extension RPC handlers to the production pool/provider. |
| 43 | Fixed RethApi to the production provider. |
| 44 | Fixed witness RPC to the production provider. |
| 45 | Fixed metering RPC to the production provider. |
| 46 | Fixed payload builder, generator, job, and service client/pool types. |
| 47 | Removed PayloadJob trait and made job operations inherent; job metadata is concrete too. |
| 48 | Fixed engine tree/state-provider builders/basic validator and prewarm/state-root jobs to the production provider. |
| 49 | Flattened Ethereum and Base transaction validators into BaseTransactionValidator and its builder, retaining common protocol validation. |
| 50 | Replaced generic validated-transaction extensions with TransactionValidity. Legacy JSON and forwarding with validity disabled retain their behavior. |

Database, inspector, oracle/provider interfaces in proof code, and actual
heterogeneous forwarding requests retain useful abstraction boundaries.
Tests that previously relied on generic mock engine/RPC providers now use
temporary production providers. Fresh Docker devnet validation, including safe-head agreement and RPC-node
restart recovery, is recorded separately from the earlier historical run.
