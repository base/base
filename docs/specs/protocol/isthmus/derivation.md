# Isthumus L2 Chain Derivation Changes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Network upgrade automation transactions](#network-upgrade-automation-transactions)
  - [EIP-2935 Contract Deployment](#eip-2935-contract-deployment)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

# Network upgrade automation transactions

The Isthumus hardfork activation block contains the following transactions, in this order:

- L1 Attributes Transaction
- User deposits from L1
- Network Upgrade Transactions
  - EIP-2935 Contract Deployment

To not modify or interrupt the system behavior around gas computation, this block will not include any sequenced
transactions by setting `noTxPool: true`.

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
