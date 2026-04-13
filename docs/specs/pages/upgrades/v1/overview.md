# V1

## Summary

:::warning
Only `base-consensus` and `base-reth-node` will support the Base V1 hardfork. If you are running
`op-node`, `op-geth` or any other clients you will need to update prior to the activation date.
:::

- Add Osaka Support
- Simplify Flashblocks Websocket Format
- Enable a new multi-proof system for faster withdrawals and a path to stronger decentralization
- Only Base Node Reth / Base Consensus will be supported

## Activation Timestamps

| Network   | Activation timestamp                   |
| --------- | -------------------------------------- |
| `mainnet` | `1777914000` (2026-05-04 17:00:00 UTC) |
| `sepolia` | `1776708000` (2026-04-20 18:00:00 UTC) |

## Execution Layer

- [EIP-7823: Upper-Bound MODEXP](/upgrades/v1/exec-engine#upper-bound-modexp)
- [EIP-7825: Transaction Gas Limit Cap](/upgrades/v1/exec-engine#transaction-gas-limit-cap)
- [EIP-7883: MODEXP Gas Cost Increase](/upgrades/v1/exec-engine#modexp-gas-cost-increase)
- [EIP-7939: CLZ Opcode](/upgrades/v1/exec-engine#clz-opcode)
- [EIP-7951: secp256r1 Precompile](/upgrades/v1/exec-engine#secp256r1-precompile-gas-cost)
- [EIP-7642: eth/69](/upgrades/v1/exec-engine#eth69)
- [EIP-7910: eth_config RPC Method](/upgrades/v1/exec-engine#eth_config-rpc-method)
- [Remove Account Balances & Receipts](/upgrades/v1/exec-engine#remove-account-balances--receipts)

## Proofs

- [Proof System](/upgrades/v1/proofs)
- [New/Changed Onchain Components](/upgrades/v1/proofs#newchanged-onchain-components)
- [Proposer](/upgrades/v1/proofs#proposer)
- [Challenger](/upgrades/v1/proofs#challenger)
- [TEE Provers](/upgrades/v1/proofs#tee-provers)
- [ZK Provers](/upgrades/v1/proofs#zk-provers)
- [Prover Registrar](/upgrades/v1/proofs#prover-registrar)
