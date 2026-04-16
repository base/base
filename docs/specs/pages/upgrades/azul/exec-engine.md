# Azul: Execution Engine

## EVM Changes

### Transaction Gas Limit Cap

[EIP-7825](https://eips.ethereum.org/EIPS/eip-7825) introduces a protocol-level maximum gas limit
of 16,777,216 (2^24) per transaction. Transactions exceeding this cap are rejected during validation.

Base adopts the same cap as L1 to maximize Ethereum equivalence.

:::note
Deposit transactions will be exempt from the transaction gas limit cap. They are already limited to [20,000,000 gas][gas-market] as that is the most
gas that can be included in an L1 block.
:::


[gas-market]: ../../protocol/bridging/deposits.md#default-values

### Upper-Bound MODEXP

[EIP-7823](https://eips.ethereum.org/EIPS/eip-7823) caps MODEXP precompile inputs to a maximum of
1024 bytes per field. Calls with larger inputs are rejected.

### MODEXP Gas Cost Increase

[EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) raises the MODEXP precompile minimum gas cost
from 200 to 500 and triples the general cost calculation.

### CLZ Opcode

[EIP-7939](https://eips.ethereum.org/EIPS/eip-7939) adds a new `CLZ` opcode that counts the number
of leading zero bits in a 256-bit word, returning 256 if the input is zero.

### secp256r1 Precompile Gas Cost

[EIP-7951](https://eips.ethereum.org/EIPS/eip-7951) specifies the secp256r1 precompile at address `0x100`
with a gas cost of 3,450.

Base already has the `p256Verify` precompile at the same address (added in Fjord via
[RIP-7212](https://github.com/ethereum/RIPs/blob/master/RIPS/rip-7212.md)) with a gas cost of 3,450.
From Azul, the gas cost increases to 6,900 to match the L1 gas cost specified in EIP-7951, maintaining
strict equivalence with L1 precompile pricing.

## Networking Changes

### eth/69

[EIP-7642](https://eips.ethereum.org/EIPS/eip-7642) updates the Ethereum wire protocol to version 69,
removing legacy fields from the `Status` message and simplifying the handshake.

### Remove Account Balances & Receipts

The `FlashblocksMetadata` payload transmitted over the Flashblocks WebSocket is simplified in Azul.
The `new_account_balances` and `receipts` fields are removed. The `access_list` field remains but
will not be populated in Azul.

**Before:**

```json
{
  "block_number": 43403718,
  "new_account_balances": {
    "0x4200000000000000000000000000000000000006": "0x35277a9715c6df1c99de"
  },
  "receipts": {
    "0x1ef9be45b3f7d44de9d98767ddb7c0e330b21777b67a3c79d469be9ffab091dd": {
      "cumulativeGasUsed": "0x177d7bd",
      "logs": [],
      "status": "0x1",
      "type": "0x2"
    }
  },
  "access_list": null
}
```

**After:**

```json
{
  "block_number": 43403718,
  "access_list": null
}
```

## RPC Changes

### Engine API Usage

At and after Azul activation, block production and import use the following Engine API methods:

- `engine_forkchoiceUpdatedV3` for starting block builds and forkchoice synchronization.
- `engine_getPayloadV5` for fetching built payloads.
- `engine_newPayloadV4` for importing payloads into the execution engine.

`engine_getPayloadV5` returns a V5 envelope, but the contained execution payload is still V4-shaped.
As a result, payload insertion continues through `engine_newPayloadV4` (there is no `engine_newPayloadV5`
path used by Base Azul clients).

Azul constraints for this flow:

- Blob-related Engine API inputs are constrained to empty values:
  - `expectedBlobVersionedHashes` MUST be an empty array.
  - `blobsBundle` in `engine_getPayloadV5` responses is expected to be empty.
- `executionRequests` in `engine_newPayloadV4` MUST be an empty array.

### eth_config RPC Method

[EIP-7910](https://eips.ethereum.org/EIPS/eip-7910) introduces the `eth_config` JSON-RPC method,
which returns chain configuration parameters such as fork activation timestamps.

Base Azul exposes `eth_config` using the standard EIP-7910 response schema.

The Base-specific behavior is:

- `blobSchedule` is always returned as zeroed values for `current`, `next`, and `last`.
  Base does not support native blob transactions, so it must not advertise synthetic Ethereum blob
  schedule defaults.
- `precompiles` reflects the active EVM precompile set for that fork. This includes the standard
  Ethereum precompiles plus any Base-active additions documented in the
  [precompiles specification](../../protocol/execution/evm/precompiles.md).
- `systemContracts` is limited to the contracts representable by EIP-7910. On Base this means:
  - `BEACON_ROOTS_ADDRESS` is included once Ecotone is active.
  - `HISTORY_STORAGE_ADDRESS` is included once Isthmus is active.
  - `DEPOSIT_CONTRACT_ADDRESS`, `CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS`, and
    `WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS` are omitted.

Base-specific predeploys and other OP Stack system contracts documented in the
[predeploys specification](../../protocol/execution/evm/predeploys.md) are not serialized into
`eth_config` unless they are part of the EIP-7910 schema.
