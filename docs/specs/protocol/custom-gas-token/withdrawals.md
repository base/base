# Withdrawals

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [The L2ToL1MessagePasser Contract](#the-l2tol1messagepasser-contract)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## The L2ToL1MessagePasser Contract

The `initiateWithdrawal` function MUST revert if `isFeatureEnabled[Features.CUSTOM_GAS_TOKEN]`
is `true` and `msg.value > 0`.
