# `base-validated-tx-rpc`

RPC endpoint for inserting pre-validated transactions into the builder transaction pool.

## Overview

This crate provides the `base_insertValidatedTransactions` RPC endpoint that accepts transactions
which have already been validated by the caller (e.g., a mempool node). The transactions include
the recovered sender address, allowing the builder to trust the validation and insert directly
into the pool.

## RPC Method

### `base_insertValidatedTransactions`

Inserts pre-validated transactions into the transaction pool.

**Parameters:**
- `txs`: Array of validated transactions with base64-encoded calldata

**Returns:**
- Array of results, one per transaction, indicating success or failure

## Transaction Format

Transactions use base64 encoding for the `input` (calldata) field instead of hex for efficiency:

```json
{
  "from": "0x...",
  "tx_type": 2,
  "chain_id": 8453,
  "nonce": 42,
  "to": "0x...",
  "value": "0x0",
  "gas_limit": 21000,
  "max_fee_per_gas": 1000000000,
  "max_priority_fee_per_gas": 100000000,
  "input": "SGVsbG8gV29ybGQ=",
  "signature": {
    "v": 0,
    "r": "0x...",
    "s": "0x..."
  }
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
