# Granite L2 Chain Derivation Changes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Protocol Parameter Changes](#protocol-parameter-changes)
- [Reduce Channel Timeout](#reduce-channel-timeout)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Protocol Parameter Changes

The following table gives an overview of the changes in parameters.

| Parameter | Pre-Granite (default) value | Granite value | Notes |
| --------- | ------------------------- | ----------- | ----- |
| `CHANNEL_TIMEOUT` | 300 | 50 | Protocol Constant is reduced. |

## Reduce Channel Timeout

With Granite, the `CHANNEL_TIMEOUT` is reduced from 300 to 50 L1 Blocks.
The new rule activation timestamp is based on the blocktime of the L1 block that the channel frame is included.
