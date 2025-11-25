# Optimism Portal

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Definitions](#definitions)
  - [Custom Gas Token Flag](#custom-gas-token-flag)
- [Rationale](#rationale)
- [Function Specification](#function-specification)
  - [Custom Gas Token Detection](#custom-gas-token-detection)
  - [donateETH](#donateeth)
  - [depositTransaction](#deposittransaction)
- [Security Considerations](#security-considerations)
  - [Custom Gas Token Flag Immutability](#custom-gas-token-flag-immutability)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Definitions

### Custom Gas Token Flag

The **Custom Gas Token Flag** is determined by querying the SystemConfig contract's feature flags.
The OptimismPortal determines if Custom Gas Token mode is active by checking if the
`CUSTOM_GAS_TOKEN` feature is enabled in the SystemConfig contract, rather than maintaining
its own local storage for this flag.

## Rationale

The OptimismPortal's deposit logic must revert only for deposits with ETH value when Custom Gas Token mode
is enabled to prevent ETH from acting as the native asset. Deposits without value (msg.value == 0) are
permitted. Since the client side does not discern native asset supply creation, allowing ETH deposits with
value would incorrectly imply that ETH can be minted in the chain.

The `donateETH` function is permitted to accept ETH even in Custom Gas Token mode, but such donations
cannot be withdrawn through normal withdrawal mechanisms due to the restrictions on value-bearing withdrawals.

## Function Specification

### Custom Gas Token Detection

The OptimismPortal does not expose a public `isCustomGasToken()` function. Instead, it uses an internal
`_isUsingCustomGasToken()` method that queries the SystemConfig contract to determine if the
`CUSTOM_GAS_TOKEN` feature flag is enabled. External consumers should query the SystemConfig directly
to determine the Custom Gas Token mode status.

### donateETH

- Accepts ETH value without triggering a deposit to L2.

### depositTransaction

- MUST revert if Custom Gas Token mode is active and `msg.value > 0`.

## Security Considerations

### Custom Gas Token Flag Immutability

The Custom Gas Token mode is controlled through the SystemConfig contract's feature flags.
Once the `CUSTOM_GAS_TOKEN` feature is enabled in the SystemConfig, it should not be
disabled in subsequent updates. Changing from custom gas token mode back to ETH mode could
create inconsistencies in the chain's gas token handling and potentially lead to security
vulnerabilities. The feature flag represents a fundamental configuration of the chain's gas
token mechanism and should remain consistent throughout the chain's lifetime.
