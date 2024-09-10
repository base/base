# Ecotone Network Upgrade

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Execution Layer](#execution-layer)
- [Consensus Layer](#consensus-layer)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The Ecotone upgrade contains the Dencun upgrade from L1, and adopts EIP-4844 blobs for data-availability.

## Execution Layer

- Cancun (Execution Layer):
  - [EIP-1153: Transient storage opcodes](https://eips.ethereum.org/EIPS/eip-1153)
  - [EIP-4844: Shard Blob Transactions](https://eips.ethereum.org/EIPS/eip-4844)
    - [Blob transactions are disabled](../exec-engine.md#ecotone-disable-blob-transactions)
  - [EIP-4788: Beacon block root in the EVM](https://eips.ethereum.org/EIPS/eip-4788)
    - [The L1 beacon block root is embedded into L2](../exec-engine.md#ecotone-beacon-block-root)
    - [The Beacon roots contract deployment is automated](../derivation.md#ecotone-beacon-block-roots-contract-deployment-eip-4788)
  - [EIP-5656: MCOPY - Memory copying instruction](https://eips.ethereum.org/EIPS/eip-5656)
  - [EIP-6780: SELFDESTRUCT only in same transaction](https://eips.ethereum.org/EIPS/eip-6780)
  - [EIP-7516: BLOBBASEFEE opcode](https://eips.ethereum.org/EIPS/eip-7516)
    - [BLOBBASEFEE always pushes 1 onto the stack](../exec-engine.md#ecotone-disable-blob-transactions)
- Deneb (Consensus Layer): _not applicable to L2_
  - [EIP-7044: Perpetually Valid Signed Voluntary Exits](https://eips.ethereum.org/EIPS/eip-7044)
  - [EIP-7045: Increase Max Attestation Inclusion Slot](https://eips.ethereum.org/EIPS/eip-7045)
  - [EIP-7514: Add Max Epoch Churn Limit](https://eips.ethereum.org/EIPS/eip-7514)

## Consensus Layer

[retrieval]: ../derivation.md#ecotone-blob-retrieval
[predeploy]: ./l1-attributes.md#ecotone-l1block-upgrade

- Blobs Data Availability: support blobs DA the [L1 Data-retrieval stage][retrieval].
- Rollup fee update: support blobs DA in
  [L1 Data Fee computation](../exec-engine.md#ecotone-l1-cost-fee-changes-eip-4844-da)
- Auto-upgrading and extension of the [L1 Attributes Predeployed Contract][predeploy]
  (also known as `L1Block` predeploy)
