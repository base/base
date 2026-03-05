# L2 Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [EVM Changes](#evm-changes)
  - [`bn256Pairing` precompile input restriction](#bn256pairing-precompile-input-restriction)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## EVM Changes

### `bn256Pairing` precompile input restriction

The `bn256Pairing` precompile execution has additional validation on its input.
The precompile reverts if its input is larger than `112687` bytes.
This is the input size that consumes approximately 20 M gas given the latest `bn256Pairing` gas schedule on L2.
