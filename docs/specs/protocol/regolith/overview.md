# Regolith

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

[deposits]: ../deposits.md
[exec-engine]: ../exec-engine.md

## Overview

The Regolith upgrade, named after a material best described as "deposited dust on top of a layer of bedrock",
implements minor changes to deposit processing, based on reports of the Sherlock Audit-contest and findings in
the Bedrock Optimism Goerli testnet.

Summary of changes:

- The `isSystemTx` boolean is disabled, system transactions now use the same gas accounting rules as regular deposits.
- The actual deposit gas-usage is recorded in the receipt of the deposit transaction,
  and subtracted from the L2 block gas-pool.
  Unused gas of deposits is not refunded with ETH however, as it is burned on L1.
- The `nonce` value of the deposit sender account, before the transaction state-transition, is recorded in a new
  optional field (`depositNonce`), extending the transaction receipt (i.e. not present in pre-Regolith receipts).
- The recorded deposit `nonce` is used to correct the transaction and receipt metadata in RPC responses,
  including the `contractAddress` field of deposits that deploy contracts.
- The `gas` and `depositNonce` data is committed to as part of the consensus-representation of the receipt,
  enabling the data to be safely synced between independent L2 nodes.
- The L1-cost function was corrected to more closely match pre-Bedrock behavior.

The [deposit specification][deposits] specifies the deposit changes of the Regolith upgrade in more detail.
The [execution engine specification][exec-engine] specifies the L1 cost function difference.

The Regolith upgrade uses a _L2 block-timestamp_ activation-rule, and is specified in both the
rollup-node (`regolith_time`) and execution engine (`config.regolithTime`).
