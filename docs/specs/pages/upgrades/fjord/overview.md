# Fjord

## Activation Timestamps

| Network | Activation timestamp |
| --- | --- |
| `mainnet` | `1720627201` (2024-07-10 16:00:01 UTC) |
| `sepolia` | `1716998400` (2024-05-29 16:00:00 UTC) |

## Execution Layer

- [RIP-7212: Precompile for secp256r1 Curve Support](/protocol/execution/evm/precompiles#P256VERIFY)
- [FastLZ compression for L1 data fee calculation](/upgrades/fjord/exec-engine#fees)
- [Deprecate the `getL1GasUsed` method on the `GasPriceOracle` contract](/upgrades/fjord/predeploys#l1-gas-usage-estimation)
- [Deprecate the `L1GasUsed` field on the transaction receipt](/upgrades/fjord/exec-engine#l1-gas-usage-estimation)

## Consensus Layer

- [Constant maximum sequencer drift](/upgrades/fjord/derivation#constant-maximum-sequencer-drift)
- [Brotli channel compression](/upgrades/fjord/derivation#brotli-channel-compression)
- [Increase Max Bytes Per Channel and Max Channel Bank Size](/upgrades/fjord/derivation#increasing-max_rlp_bytes_per_channel-and-max_channel_bank_size)
