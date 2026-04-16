# Azul

## Summary

:::warning
Only `base-consensus` and `base-reth-node` will support the Base Azul hardfork. If you are running
`op-node`, `op-geth` or any other clients you will need to update prior to the activation date.
:::

- Add Osaka Support
- Simplify Flashblocks Websocket Format
- Enable a new multi-proof system for faster withdrawals and a path to stronger decentralization
- Only Base Node Reth / Base Consensus will be supported

## Activation Timestamps

| Network   | Activation timestamp                   |
| --------- | -------------------------------------- |
| `mainnet` | `1778695200` (2026-05-13 18:00:00 UTC) |
| `sepolia` | `1776708000` (2026-04-20 18:00:00 UTC) |

## Execution Layer

- [EIP-7823: Upper-Bound MODEXP](/upgrades/azul/exec-engine#upper-bound-modexp)
- [EIP-7825: Transaction Gas Limit Cap](/upgrades/azul/exec-engine#transaction-gas-limit-cap)
- [EIP-7883: MODEXP Gas Cost Increase](/upgrades/azul/exec-engine#modexp-gas-cost-increase)
- [EIP-7939: CLZ Opcode](/upgrades/azul/exec-engine#clz-opcode)
- [EIP-7951: secp256r1 Precompile](/upgrades/azul/exec-engine#secp256r1-precompile-gas-cost)
- [EIP-7642: eth/69](/upgrades/azul/exec-engine#eth69)
- [EIP-7910: eth_config RPC Method](/upgrades/azul/exec-engine#eth_config-rpc-method)
- [Remove Account Balances & Receipts](/upgrades/azul/exec-engine#remove-account-balances--receipts)

## Proofs

- [Proof System](/upgrades/azul/proofs)
- [New/Changed Onchain Components](/upgrades/azul/proofs#newchanged-onchain-components)
- [Proposer](/upgrades/azul/proofs#proposer)
- [Challenger](/upgrades/azul/proofs#challenger)
- [TEE Provers](/upgrades/azul/proofs#tee-provers)
- [ZK Provers](/upgrades/azul/proofs#zk-provers)
- [Prover Registrar](/upgrades/azul/proofs#prover-registrar)
