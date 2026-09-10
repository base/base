# Consolidation validation

Validated on 2026-09-10. The workspace layout and ownership are described in
[the architecture overview](README.md).

## Build and policy checks

- `cargo check --workspace --all-targets` passed after the final package moves.
- `just check crate-deps` passed, including nine policy regression tests.
- The proof execution client, witness MPT, witness preimage and protocol types
  passed a no-default-features check for
  `riscv32imac-unknown-none-elf` with the custom getrandom backend.
- Documentation warnings remain excluded from this cleanup. Cargo also reports
  upstream future-compatibility notices for proc-macro-error2 and russh.
- The generated SVG was parsed successfully and contains 109 package nodes,
  nine ownership clusters and 768 internal edges.

Each merge was validated separately before its commit. Checks included the
affected EVM/crypto backends, provider/trie/database behavior, RPC and CLI
interfaces, pure proof targets, and real Postgres, S3/MinIO and Docker system
fixtures where relevant. The last block API specialization passed 379 chain
type tests and 285 provider tests. Binary naming changes passed 57 tests, with
one pre-existing ignored test. The extracted witness-diff binary builds and
prints its existing CLI help.

The final block-consumer regression run passed 165 execution-driver tests,
518 transaction-pool tests and 75 node-service tests (758 total). Three
transaction-pool tests remain ignored.

## Fresh Docker devnet

`just devnet up` rebuilt the Base image using Rust 1.96.0 and started the
complete Docker stack. The Base image ID was
`sha256:0bcd27b20b3d439e29352c9141cd833afb26e7840533bd6e51c655ac7e6a0b28`.

A value transfer submitted through the client RPC succeeded in block 17:

`0x49a7f1a4aa93da00e7250fb8d314f755476d33510d47f37ae31b8595405a0bcf`

All six nodes returned the same block hash and successful receipt, and all six
advanced their safe head past that transaction. Initial unsafe heads were 15
and initial safe heads were zero.

| Node | Observed unsafe head | Observed safe head |
|---|---:|---:|
| Builder | 1074 | 1002 |
| Client | 1074 | 102 |
| RPC | 1074 | 42 |
| Sequencer 1 | 1074 | 1002 |
| Sequencer 2 | 1074 | 1002 |
| Shadow validator | 1074 | 99 |

Follower safe heads lag because their configured L1 confirmation delay is
preserved. The six nodes, both batchers and bootnode had no ERROR/FATAL/panic
log entries during the run. Startup warnings about empty peer caches,
ephemeral peer keys and conductor readiness were present; the conductor
recovered and block production continued. Discovery also reports an empty
closest-peer set in this isolated devnet.

The devnet remains running. These observations validate this run, not every
possible deployment or runtime feature combination.
