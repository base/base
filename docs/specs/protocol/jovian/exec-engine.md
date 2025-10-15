# Jovian: Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Minimum Base Fee](#minimum-base-fee)
  - [Minimum Base Fee in Block Header](#minimum-base-fee-in-block-header)
  - [Minimum Base Fee in `PayloadAttributesV3`](#minimum-base-fee-in-payloadattributesv3)
  - [Rationale](#rationale)
- [DA Footprint Block Limit](#da-footprint-block-limit)
  - [Scalar loading](#scalar-loading)
  - [Rationale](#rationale-1)
- [Operator Fee](#operator-fee)
  - [Fee Formula Update](#fee-formula-update)
  - [Maximum value](#maximum-value)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Minimum Base Fee

Jovian introduces a
[configurable minimum base fee](https://github.com/ethereum-optimism/design-docs/blob/main/protocol/minimum-base-fee.md)
to reduce the duration of priority-fee auctions on OP Stack chains.

The minimum base fee is configured via `SystemConfig` (see `./system-config.md`) and enforced by the execution engine
via the block header `extraData` encoding and the Engine API `PayloadAttributesV3` parameters.

### Minimum Base Fee in Block Header

Like [Holocene's dynamic EIP-1559 parameters](../holocene/exec-engine.md#dynamic-eip-1559-parameters), Jovian encodes
fee parameters in the `extraData` field of each L2 block header. The format is extended to include an additional
`u64` field for the minimum base fee in wei.

| Name                | Type               | Byte Offset |
| ------------------- | ------------------ | ----------- |
| `minBaseFee`        | `u64 (big-endian)` | `[9, 17)`   |

Constraints:

- `version` MUST be `1` (incremented from Holocene's `0`).
- There MUST NOT be any data beyond these 17 bytes.

The `minBaseFee` field is an absolute minimum expressed in wei. During base fee computation, if the
computed `baseFee` is less than `minBaseFee`, it MUST be clamped to `minBaseFee`.

```javascript
if (baseFee < minBaseFee) {
  baseFee = minBaseFee
}
```

Note: `extraData` has a maximum capacity of 32 bytes (to fit the L1 beacon-chain `extraData` type) and may be
extended by future upgrades.

### Minimum Base Fee in `PayloadAttributesV3`

The Engine API [`PayloadAttributesV3`](../exec-engine.md#extended-payloadattributesv3) is extended with a new
field `minBaseFee`. The existing `eip1559Params` remains 8 bytes (Holocene format).

```text
PayloadAttributesV3: {
    timestamp: QUANTITY
    prevRandao: DATA (32 bytes)
    suggestedFeeRecipient: DATA (20 bytes)
    withdrawals: array of WithdrawalV1
    parentBeaconBlockRoot: DATA (32 bytes)
    transactions: array of DATA
    noTxPool: bool
    gasLimit: QUANTITY or null
    eip1559Params: DATA (8 bytes) or null
    minBaseFee: QUANTITY or null
}
```

The `minBaseFee` MUST be `null` prior to the Jovian fork, and MUST be non-`null` after the Jovian fork.

### Rationale

As with [Holocene's dynamic EIP-1559 parameters](../holocene/exec-engine.md#rationale), placing the
minimum base fee in the block header allows us to avoid reaching into the state during block sealing.
This retains the purity of the function that computes the next block's base fee from its parent block
header, while still allowing them to be dynamically configured. Dynamic configuration is handled
similarly to `gasLimit`, with the derivation pipeline providing the appropriate `SystemConfig`
contract values to the block builder via `PayloadAttributesV3` parameters.

## DA Footprint Block Limit

A _DA footprint block limit_ is introduced to limit the total amount of estimated compressed
transaction data that can fit into a block.
For each transaction, a new resource called DA footprint is tracked, next to its gas usage.
It is scaled to the gas dimension so that its block total can also be limited by
the block gas limit, like a block's total gas usage.

Let a block's `daFootprint` be defined as follows:

```python
def daFootprint(block: Block) -> int:
  daFootprint = 0

  for tx in block.transactions:
      if tx.type == DEPOSIT_TX_TYPE:
          continue

      daUsageEstimate = max(
          minTransactionSize,
          (intercept + fastlzCoef * tx.fastlzSize) // 1e6
      )
      daFootprint += daUsageEstimate * daFootprintGasScalar

  return daFootprint 
```

where `intercept`, `minTransactionSize`, `fastlzCoef` and `fastlzSize`
are defined in the [Fjord specs](../fjord/exec-engine.md), `DEPOSIT_TX_TYPE` is `0x7E`,
and `//` represents integer floor division.

From Jovian, the `blobGasUsed` property of each block header is set to that block's `daFootprint`. Note that pre-Jovian,
since Ecotone, it was set to 0, as OP Stack chains don't support blobs. It is now repurposed to store the DA footprint.

During block building and header validation, it must be guaranteed and checked, respectively, that the block's
`daFootprint` stays below the `gasLimit`, just like the `gasUsed` property.
Note that this implies that blocks may have no more than `gasLimit/daFootprintGasScalar` total estimated DA usage bytes.

Furthermore, from Jovian, the base fee update calculation now uses `gasMetered := max(gasUsed, blobGasUsed)`
in place of the `gasUsed` value used before.
As a result, blocks with high DA usage may cause the base fee to increase in subsequent blocks.

### Scalar loading

The `daFootprintGasScalar` is loaded in a similar way to the `operatorFeeScalar` and `operatorFeeConstant`
[included](../isthmus/exec-engine.md#operator-fee) in the Isthmus fork. It can be read in two interchangable ways:

- read from the deposited L1 attributes (`daFootprintGasScalar`) of the current L2 block
(decoded according to the [jovian schema](./l1-attributes.md))
- read from the L1 Block Info contract (`0x4200000000000000000000000000000000000015`)
  - using the solidity getter function `daFootprintGasScalar`
  - using a direct storage-read: big-endian `uint16` in slot `8` at offset `12`.

It takes on a default value as described in the section on [L1 Attributes](./l1-attributes.md).

### Rationale

While the current L1 fee mechanism charges for DA usage based on an estimate of the DA footprint of a transaction, no
protocol mechanism currently reflects the limited available _DA throughput on L1_. E.g. on Ethereum L1 with Pectra
enabled, the available blob throughput is `~96 kB/s` (with a target of `~64 kB/s`), but the calldata floor gas price of
`40` for calldata-heavy L2 transactions allows for more incompressible transaction data to be included on most OP Stack
chains than the Ethereum blob space could handle. This is currently mitigated at the policy level by batcher-sequencer
throttling: a mechanism which artificially constricts block building. This can cause base fees to fall, which implies
unnecessary losses for chain operators and a negative user experience (transaction inclusion delays, priority fee
auctions). So hard-limiting a block's DA footprint in a way that also influences the base fee mitigates the
aforementioned problems of policy-based solutions.

## Operator Fee

### Fee Formula Update

Jovian updates the operator fee calculation so that higher fees may be charged.
Starting at the Jovian activation, the operator fee MUST be computed as:

$$
\text{operatorFee} = (\text{gas} \times \text{operatorFeeScalar} \times 100) + \text{operatorFeeConstant}
$$

The effective per-gas scalar applied is therefore `100 * operatorFeeScalar`. Otherwise, the data types and operator fee
semantics described in the [Isthmus spec](../isthmus/exec-engine.md#operator-fee) continue to apply.

### Maximum value

With the new formula, the operator fee's maximum value has 103 bits:

$$
\text{operatorFee}_{\text{max}} = (\text{uint64}_{\text{max}} \times \text{uint32}_{\text{max}} \times 100) +
\text{uint64}_{\text{max}} \approx 7.924660923989131 \times 10^{30}
$$

Implementations that use `uint256` for intermediate arithmetic do not need additional overflow checks.
