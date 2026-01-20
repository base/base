# `base-reth-metering`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Metering RPC for Base node. Provides RPC methods for measuring transaction and block execution timing.

## RPC Methods

### `base_meterBundle`

Simulates and meters a bundle of transactions.

**Parameters:**
- `bundle`: Bundle object containing transactions to simulate

**Returns:**
- `MeterBundleResponse`: Contains per-transaction results, total gas used, execution times

### `base_meterBlockByHash`

Re-executes a block by hash and returns timing metrics.

**Parameters:**
- `hash`: Block hash (B256)

**Returns:**
- `MeterBlockResponse`: Contains timing breakdown for signer recovery, EVM execution, and state root calculation

### `base_meterBlockByNumber`

Re-executes a block by number and returns timing metrics.

**Parameters:**
- `number`: Block number or tag (e.g., "latest")

**Returns:**
- `MeterBlockResponse`: Contains timing breakdown for signer recovery, EVM execution, and state root calculation
