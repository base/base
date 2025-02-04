# Isthmus L2 Chain Derivation Changes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Network upgrade automation transactions](#network-upgrade-automation-transactions)
  - [L1Block deployment](#l1block-deployment)
  - [GasPriceOracle deployment](#gaspriceoracle-deployment)
  - [Operator fee vault deployment](#operator-fee-vault-deployment)
  - [L1Block Proxy Update](#l1block-proxy-update)
  - [GasPriceOracle Proxy Update](#gaspriceoracle-proxy-update)
  - [OperatorFeeVault Proxy Update](#operatorfeevault-proxy-update)
  - [GasPriceOracle Enable Isthmus](#gaspriceoracle-enable-isthmus)
  - [EIP-2935 Contract Deployment](#eip-2935-contract-deployment)
- [Span Batch Updates](#span-batch-updates)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

# Network upgrade automation transactions

The Isthumus hardfork activation block contains the following transactions, in this order:

- L1 Attributes Transaction
- User deposits from L1
- Network Upgrade Transactions
  - L1Block deployment
  - GasPriceOracle deployment
  - Operator Fee vault deployment
  - Update L1Block Proxy ERC-1967 Implementation
  - Update GasPriceOracle Proxy ERC-1967 Implementation
  - Update Operator Fee vault Proxy ERC-1967 Implementation
  - GasPriceOracle Enable Isthmus
  - EIP-2935 Contract Deployment

To not modify or interrupt the system behavior around gas computation, this block will not include any sequenced
transactions by setting `noTxPool: true`.

## L1Block deployment

The `L1Block` contract is upgraded to support the Isthmus operator fee feature.

A deposit transaction is derived with the following attributes:

- `from`: `0x4210000000000000000000000000000000000003`
- `to`: `null`
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `425,000`
- `data`: `0x60806040523480156100105...` ([full bytecode](../../static/bytecode/isthmus-l1-block-deployment.txt))
- `sourceHash`: `0x3b2d0821ca2411ad5cd3595804d1213d15737188ae4cbd58aa19c821a6c211bf`,
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: L1 Block Deployment"

This results in the Isthmus L1Block contract being deployed to `0xFf256497D61dcd71a9e9Ff43967C13fdE1F72D12`, to verify:

```bash
cast compute-address --nonce=0 0x4210000000000000000000000000000000000003
Computed Address: 0xFf256497D61dcd71a9e9Ff43967C13fdE1F72D12
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: L1 Block Deployment"))
# 0x3b2d0821ca2411ad5cd3595804d1213d15737188ae4cbd58aa19c821a6c211bf
```

Verify `data`:

```bash
git checkout TODO
make build-contracts
jq -r ".bytecode.object" packages/contracts-bedrock/forge-artifacts/L1Block.sol/L1Block.json
```

This transaction MUST deploy a contract with the following code hash
`TODO`.

## GasPriceOracle deployment

The `GasPriceOracle` contract is also upgraded to support the Isthmus operator fee feature.

A deposit transaction is derived with the following attributes:

- `from`: `0x4210000000000000000000000000000000000004`
- `to`: `null`
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `1,625,000`
- `data`: `0x60806040523480156100105...` ([full bytecode](../../static/bytecode/isthmus-gas-price-oracle-deployment.txt))
- `sourceHash`: `0xfc70b48424763fa3fab9844253b4f8d508f91eb1f7cb11a247c9baec0afb8035`,
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: Gas Price Oracle Deployment"

This results in the Isthmus GasPriceOracle contract being deployed to `0x93e57A196454CB919193fa9946f14943cf733845`, to verify:

```bash
cast compute-address --nonce=0 0x4210000000000000000000000000000000000003
Computed Address: 0xFf256497D61dcd71a9e9Ff43967C13fdE1F72D12
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: Gas Price Oracle Deployment"))
# 0xfc70b48424763fa3fab9844253b4f8d508f91eb1f7cb11a247c9baec0afb8035
```

Verify `data`:

```bash
git checkout TODO
make build-contracts
jq -r ".bytecode.object" packages/contracts-bedrock/forge-artifacts/GasPriceOracle.sol/GasPriceOracle.json
```

This transaction MUST deploy a contract with the following code hash
`TODO`.

## Operator fee vault deployment

A new `OperatorFeeVault` contract has been created to recieve the operator fees. The contract is created
with the following arguments:

- Recipient address: The base fee vault
- Min withdrawal amount: 0
- Withdrawal network: L2

A deposit transaction is derived with the following attributes:

- `from`: `0x4210000000000000000000000000000000000005`
- `to`: `null`
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `500,000`
- `data`: `0x60806040523480156100105...` ([full bytecode](../../static/bytecode/isthmus-operator-fee-deployment.txt))
- `sourceHash`: `0x107a570d3db75e6110817eb024f09f3172657e920634111ce9875d08a16daa96`,
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: Operator Fee Vault Deployment"

This results in the Isthmus OperatorFeeVault contract being deployed to
`0x4fa2Be8cd41504037F1838BcE3bCC93bC68Ff537`, to verify:

```bash
cast compute-address --nonce=0 0x4210000000000000000000000000000000000003
Computed Address: 0x4fa2Be8cd41504037F1838BcE3bCC93bC68Ff537
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: Operator Fee Vault Deployment"))
# 0x107a570d3db75e6110817eb024f09f3172657e920634111ce9875d08a16daa96
```

Verify `data`:

```bash
git checkout TODO
make build-contracts
jq -r ".bytecode.object" packages/contracts-bedrock/forge-artifacts/OperatorFeeVault.sol/OperatorFeeVault.json
```

This transaction MUST deploy a contract with the following code hash
`TODO`.

## L1Block Proxy Update

This transaction updates the L1Block Proxy ERC-1967 implementation slot to point to the new L1Block deployment.

A deposit transaction is derived with the following attributes:

- `from`: `0x0000000000000000000000000000000000000000`
- `to`: `0x4200000000000000000000000000000000000015` (L1Block Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `50,000`
- `data`: `0x3659cfe6000000000000000000000000ff256497d61dcd71a9e9ff43967c13fde1f72d12`
- `sourceHash`: `0xebe8b5cb10ca47e0d8bda8f5355f2d66711a54ddeb0ef1d30e29418c9bf17a0e`
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: L1 Block Proxy Update"

Verify data:

```bash
cast concat-hex $(cast sig "upgradeTo(address)") $(cast abi-encode "upgradeTo(address)" 0xff256497d61dcd71a9e9ff43967c13fde1f72d12)
0x3659cfe6000000000000000000000000ff256497d61dcd71a9e9ff43967c13fde1f72d12
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: L1 Block Proxy Update"))
# 0xebe8b5cb10ca47e0d8bda8f5355f2d66711a54ddeb0ef1d30e29418c9bf17a0e
```

## GasPriceOracle Proxy Update

This transaction updates the GasPriceOracle Proxy ERC-1967 implementation slot to point to the new GasPriceOracle
deployment.

A deposit transaction is derived with the following attributes:

- `from`: `0x0000000000000000000000000000000000000000`
- `to`: `0x420000000000000000000000000000000000000F` (Gas Price Oracle Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `50,000`
- `data`: `0x3659cfe600000000000000000000000093e57a196454cb919193fa9946f14943cf733845`
- `sourceHash`: `0xecf2d9161d26c54eda6b7bfdd9142719b1e1199a6e5641468d1bf705bc531ab0`
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: Gas Price Oracle Proxy Update"`

Verify data:

```bash
cast concat-hex $(cast sig "upgradeTo(address)") $(cast abi-encode "upgradeTo(address)" 0x93e57a196454cb919193fa9946f14943cf733845)
0x3659cfe600000000000000000000000093e57a196454cb919193fa9946f14943cf733845
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: Gas Price Oracle Proxy Update"))
# 0xecf2d9161d26c54eda6b7bfdd9142719b1e1199a6e5641468d1bf705bc531ab0
```

## OperatorFeeVault Proxy Update

This transaction updates the GasPriceOracle Proxy ERC-1967 implementation slot to point to the new GasPriceOracle
deployment.

A deposit transaction is derived with the following attributes:

- `from`: `0x0000000000000000000000000000000000000000`
- `to`: `0x420000000000000000000000000000000000001B` (Operator Fee Vault Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `50,000`
- `data`: `0x3659cfe60000000000000000000000004fa2be8cd41504037f1838bce3bcc93bc68ff537`
- `sourceHash`: `0xad74e1adb877ccbe176b8fa1cc559388a16e090ddbe8b512f5b37d07d887a927`
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: Operator Fee Vault Proxy Update"`

Verify data:

```bash
cast concat-hex $(cast sig "upgradeTo(address)") $(cast abi-encode "upgradeTo(address)" 0x4fa2be8cd41504037f1838bce3bcc93bc68ff537)
0x3659cfe60000000000000000000000004fa2be8cd41504037f1838bce3bcc93bc68ff537
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: Operator Fee Vault Proxy Update"))
# 0xad74e1adb877ccbe176b8fa1cc559388a16e090ddbe8b512f5b37d07d887a927
```

## GasPriceOracle Enable Isthmus

This transaction informs the GasPriceOracle to start using the Isthmus gas calculation formula.

A deposit transaction is derived with the following attributes:

- `from`: `0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001` (Depositer Account)
- `to`: `0x420000000000000000000000000000000000000F` (Gas Price Oracle Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `90,000`
- `data`: `0x291b0383`
- `sourceHash`: `0x3ddf4b1302548dd92939826e970f260ba36167f4c25f18390a5e8b194b295319`,
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: Gas Price Oracle Set Isthmus"

Verify data:

```bash
cast sig "setIsthmus()"
0x8e98b106
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: Gas Price Oracle Set Isthmus"))
# 0x3ddf4b1302548dd92939826e970f260ba36167f4c25f18390a5e8b194b295319
```

## EIP-2935 Contract Deployment

[EIP-2935](https://eips.ethereum.org/EIPS/eip-2935) requires a contract to be deployed. To deploy this contract,
a deposit transaction is created with attributes matching the EIP:

- `from`: `0xE9f0662359Bb2c8111840eFFD73B9AFA77CbDE10`
- `to`: `null`,
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `250,000`
- `data`: `0x60538060095f395ff33373fffffffffffffffffffffffffffffffffffffffe14604657602036036042575f35600143038111604257611fff81430311604257611fff9006545f5260205ff35b5f5ffd5b5f35611fff60014303065500`,
- `sourceHash`: `0xbfb734dae514c5974ddf803e54c1bc43d5cdb4a48ae27e1d9b875a5a150b553a`
  computed with the "Upgrade-deposited" type, with `intent = "Isthmus: EIP-2935 Contract Deployment"

This results in the EIP-2935 contract being deployed to `0x0F792be4B0c0cb4DAE440Ef133E90C0eCD48CCCC`, to verify:

```bash
cast compute-address --nonce=0 0xE9f0662359Bb2c8111840eFFD73B9AFA77CbDE10
Computed Address: 0x0F792be4B0c0cb4DAE440Ef133E90C0eCD48CCCC
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Isthmus: EIP-2935 Contract Deployment"))
# 0xbfb734dae514c5974ddf803e54c1bc43d5cdb4a48ae27e1d9b875a5a150b553a
```

This transaction MUST deploy a contract with the following code hash
`0x6e49e66782037c0555897870e29fa5e552daf4719552131a0abce779daec0a5d`.

# Span Batch Updates

[Span batches](../delta/span-batches.md) are a span of consecutive L2 blocks than are batched submitted.

Span batches contain the L1 transactions and transaction types that are posted containing the span of L2 blocks.
Since [EIP-7702] introduces a new transaction type, the Span Batch must be updated to support the [EIP-7702]
transaction.

This corresponds with a new RLP-encoding of the `tx_datas` list as specified in
[the Delta span batch spec](../delta/span-batches.md), adding a new transaction type:

Transaction type `4` ([EIP-7702]):
`0x04 ++ rlp_encode(value, max_priority_fee_per_gas, max_fee_per_gas, data, access_list, authorization_list)`

The [EIP-7702] transaction extends [EIP-1559] to include a new `authorization_list` field.
`authorization_list` is an RLP-encoded list of authorization tuples.
The [EIP-7702] transaction format is as follows.

- `value`: The transaction value as a `u256`.
- `max_priority_fee_per_gas`: The maximum priority fee per gas allowed as a `u256`.
- `max_fee_per_gas`: The maximum fee per gas as a `u256`.
- `data`: The transaction data bytes.
- `access_list`: The [EIP-2930] access list.
- `authorization_list`: The [EIP-7702] signed authorization list.

Span batches with transaction type `4` should only be accepted after Isthmus is enabled.

[EIP-1559]: https://eips.ethereum.org/EIPS/eip-1559
[EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
[EIP-2930]: https://eips.ethereum.org/EIPS/eip-2930
