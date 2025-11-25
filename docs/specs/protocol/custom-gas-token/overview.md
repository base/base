# Custom Gas Token Mode

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Execution Layer](#execution-layer)
- [Consensus Layer](#consensus-layer)
- [Smart Contracts](#smart-contracts)
  - [Core L2 Smart Contracts](#core-l2-smart-contracts)
    - [Custom Gas Token](#custom-gas-token)
- [Development and Configuration](#development-and-configuration)
  - [Fresh Deployments](#fresh-deployments)
  - [Migration from Existing Implementation](#migration-from-existing-implementation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

This document is not finalized and should be considered experimental.

## Execution Layer

Custom Gas Token mode operates at the execution layer by modifying the behavior of predeploy contracts
and bridging functions. The implementation uses a bitmap-based feature flag approach that enables CGT functionality
through the `systemConfig.isFeatureEnabled(Features.CUSTOM_GAS_TOKEN)` check across various system contracts.

## Consensus Layer

CGT mode does not require consensus layer changes. The native asset used for gas fees is managed
entirely at the execution layer through smart contract logic, maintaining full compatibility with
the existing consensus mechanisms.

## Smart Contracts

- [Predeploys](./predeploys.md)
- [Bridges](./bridges.md)
- [Cross Domain Messengers](./messengers.md)
- [System Config](./system-config.md)
- [Withdrawals](./withdrawals.md)
- [Optimism Portal](./optimism-portal.md)

### Core L2 Smart Contracts

#### Custom Gas Token

The Custom Gas Token (CGT) feature allows OP Stack chains to use a native asset other than ETH as the gas
currency. This implementation introduces a streamlined approach with minimal core code intrusion through a
single `isCustomGasToken()` flag.

Key components:

- **NativeAssetLiquidity**: A predeploy contract containing pre-minted native assets, deployed only for
  CGT-enabled chains. The amount of the pre-minted liquidity is configurable up to `type(uint248).max`.
- **LiquidityController**: An owner-governed mint/burn router that manages supply control, deployed only for
  CGT-enabled chains.
- **ETH Transfer Blocking**: When CGT is enabled, all ETH transfer flows in bridging methods are disabled via
  the `systemConfig.isFeatureEnabled(Features.CUSTOM_GAS_TOKEN)` check.
- **ETH Bridging Disabled**: ETH bridging functions in `L2ToL1MessagePasser` and `OptimismPortal` MUST revert
  when CGT mode is enabled to prevent confusion about which asset is the native currency.
- **ETH as an ERC20 representation**: ETH can be bridged by wrapping it as an ERC20 token (e.g., WETH)
  and using the `StandardBridge` to mint an `OptimismMintableERC20` representation.

OP Stack chains that use a native asset other than ETH (or the native asset of the settlement layer)
introduce custom requirements that go beyond the current supply management model based on deposits and
withdrawals. This architecture decouples and winds down the native bridging for the native asset, shifting
the responsibility for supply management to the application layer. The chain operator becomes responsible
for defining and assigning meaning to the native asset, which is managed through a new set of predeployed
contracts.

This approach preserves full alignment with EVM equivalence and client-side compatibility as provided by the
standard OP Stack. No new functionalities outside the execution environment are required to make it work.

## Development and Configuration

### Fresh Deployments

For fresh CGT deployments, the following configuration process is used:

1. **L2 Genesis Configuration**: The initial liquidity amount is configured during genesis via `l2genesis.s.sol`
2. **Contract Initialization**: `NativeAssetLiquidity` and `LiquidityController` contracts are deployed with proxy support
3. **Feature Flag Setting**: The `Features.CUSTOM_GAS_TOKEN` bitmap is enabled
   to activate CGT mode across all relevant predeploys (see [System Config](./system-config.md) for details on feature flags)

### Migration from Existing Implementation

For chains migrating from older CGT implementations:

1. **Contract Funding**: Use the `fund()` function on `NativeAssetLiquidity` to provide initial liquidity
2. **Liquidity Limits**: Ensure funded amounts do not exceed the configured native asset liquidity amount
3. **Compatibility**: Migration is only supported from previous CGT implementations, not from ETH-based chains
