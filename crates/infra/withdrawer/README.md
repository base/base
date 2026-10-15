# Withdrawer configuration

`base-withdrawer` provides `WithdrawerArgs`, a clap parser, and `WithdrawerConfig`, its typed configuration. Conversion validates settings before any RPC. This library does not derive keys, contact networks, build proofs, or submit transactions.

Supply `--l1-rpc-url`, `--withdrawal`, and exactly one signing source. `--private-key` accepts a 32-byte hex key, also available through `WITHDRAWER_PRIVATE_KEY`. `--mnemonic` accepts an English BIP-39 phrase as one quoted value or separate words, also available through `WITHDRAWER_MNEMONIC`. Conversion joins the words with single spaces and validates the phrase. `--ledger` is available only with the `ledger` Cargo feature. Mnemonic and Ledger sources use `--hd-path`, defaulting to `m/44'/60'/0'/0/0`. Each path component must have an index between 0 and 2147483647, with or without a hardened suffix. Secrets have redacted debug output and parse errors. Prefer environment variables over argv to avoid exposing secrets in process listings.

`--network` selects `base-mainnet` by default, or `base-sepolia`, `op-mainnet`, or `op-sepolia`. Each preset uses fault proofs. For a custom network, omit `--network` and supply `--l2-rpc-url`, `--portal`, and either both `--dispute-game-factory` and `--anchor-state-registry`, or `--l2-output-oracle`. Addresses and withdrawal hashes require exact-length hex values. Custom contract addresses must not be zero.

Fees default to automatic estimation. `--gas-price` selects legacy fees. EIP-1559 requires both `--max-fee-per-gas` and `--max-priority-fee-per-gas`. Fees are decimal wei and must fit `u128`. `--max-gas-price` optionally caps explicit fees and later automatic estimation. `--gas-limit` overrides estimation and conflicts with `--gas-multiplier`, whose default is 1.0 and which must be finite and at least 1.0.

`--timeout` defaults to `10m`, `--max-retries` to 3, and `--retry-delay` to `2s`. Durations use humantime syntax and must be positive. At most 32 retries are allowed, and conversion rejects retry delays whose exponential backoff would overflow a Rust duration. `--dry-run` disables signing and submission in later layers.
