# Precompiles

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [P256VERIFY](#p256verify)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

[Precompiled contracts](../glossary.md#precompiled-contract-precompile) exist on OP-Stack chains at
predefined addresses. They are similar to predeploys but are implemented as native code in the EVM as opposed to
bytecode. Precompiles are used for computationally expensive operations, that would be cost prohibitive to implement
in Solidity. Where possible predeploys are preferred, as precompiles must be implemented in every execution client.

OP-Stack chains contain the [standard Ethereum precompiles](https://www.evm.codes/precompiled) as well as a small
number of additional precompiles. The following table lists each of the additional precompiles. The system version
indicates when the precompile was introduced.

| Name       | Address                                    | Introduced |
|------------| ------------------------------------------ |------------|
| P256VERIFY | 0x0000000000000000000000000000000000000100 | Fjord      |

## P256VERIFY

The `P256VERIFY` precompile performs signature verification for the secp256r1 elliptic curve. This curve has widespread
adoption. It's used by Passkeys, Apple Secure Enclave and many other systems.

It is specified as part of [RIP-7212](https://github.com/ethereum/RIPs/blob/master/RIPS/rip-7212.md) and was added to
the OP-Stack protocol in the Fjord release. The op-geth implementation is
[here](https://github.com/ethereum-optimism/op-geth/blob/optimism/core/vm/contracts.go#L1161-L1193).

Address: `0x0000000000000000000000000000000000000100`
