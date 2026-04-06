# V1

## Summary

:::warning
Only `base-consensus` and `base-reth-node` will  support the Base V1 hardfork. If you are running
`op-node`, `op-geth` or any other clients you will need to update prior to the activation date.
:::

- Add Fusaka Support
- Simplify Flashblocks Websocket Format
- Enable TEE & ZK Proofs

## Activation Timestamps

| Network | Activation timestamp |
| --- | --- |
| `mainnet` | TBD |
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

- TEE
- ZK
