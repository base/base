# Pectra Blob Schedule (Optional) Network Upgrade

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Consensus Layer](#consensus-layer)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The Pectra Blob Schedule hardfork is an optional hardfork which delays the adoption of the
Prague blob base fee update fraction until the specified time. Until that time, the Cancun
update fraction from the previous fork is retained.

Note that the activation logic for this upgrade is different to most other upgrades.
Usually, specific behavior is activated at the _hard fork timestamp_, if it is not nil,
and continues until overridden by another hardfork.
Here, specific behavior is activated for all times up to the hard fork timestamp,
if it is not nil, and then _deactivated_ at the hard fork timestamp.

## Consensus Layer

- [Derivation](./derivation.md)
