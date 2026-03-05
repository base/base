# Fjord Network Upgrade

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Execution Layer](#execution-layer)
- [Consensus Layer](#consensus-layer)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Execution Layer

- [RIP-7212: Precompile for secp256r1 Curve Support](../precompiles.md#P256VERIFY)
- [FastLZ compression for L1 data fee calculation](./exec-engine.md#fees)
- [Deprecate the `getL1GasUsed` method on the `GasPriceOracle` contract](./predeploys.md#l1-gas-usage-estimation)
- [Deprecate the `L1GasUsed` field on the transaction receipt](./exec-engine.md#l1-gas-usage-estimation)

## Consensus Layer

- [Constant maximum sequencer drift](./derivation.md#constant-maximum-sequencer-drift)
- [Brotli channel compression](./derivation.md#brotli-channel-compression)
- [Increase Max Bytes Per Channel and Max Channel Bank Size](./derivation.md#increasing-max_rlp_bytes_per_channel-and-max_channel_bank_size)
