# System Config

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Feature Flag System](#feature-flag-system)
  - [Bitmap-based Feature Detection](#bitmap-based-feature-detection)
  - [Feature Checking](#feature-checking)
- [Definitions](#definitions)
  - [Feature Flag Bitmap](#feature-flag-bitmap)
- [Function Specification](#function-specification)
  - [isFeatureEnabled](#isfeatureenabled)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Feature Flag System

### Bitmap-based Feature Detection

The CGT implementation uses a bitmap-based feature flag system that provides efficient storage and allows
multiple features to be tracked in a single storage slot.

**Implementation:**

- Uses `isFeatureEnabled[Features.CUSTOM_GAS_TOKEN]` bitmap approach
- Enables efficient checking of multiple features
- Provides gas-optimized feature detection across contracts

### Feature Checking

Contracts check for CGT mode by querying the `SystemConfig` contract:

```solidity
systemConfig.isFeatureEnabled(Features.CUSTOM_GAS_TOKEN)
```

## Definitions

### Feature Flag Bitmap

The `SystemConfig` contract contains a mapping `isFeatureEnabled` that uses bitmaps to efficiently
track multiple features including Custom Gas Token mode.

```solidity
mapping(uint256 => bool) public isFeatureEnabled;
```

## Function Specification

### isFeatureEnabled

Returns true if the specified feature is enabled, false otherwise.

```solidity
function isFeatureEnabled(uint256 _feature) external view returns (bool)
```

For Custom Gas Token mode, contracts check:

- `isFeatureEnabled[Features.CUSTOM_GAS_TOKEN]`

This provides an efficient way to query feature states across the system.
