# Canyon

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Execution Layer](#execution-layer)
- [Consensus Layer](#consensus-layer)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

[eip3651]: https://eips.ethereum.org/EIPS/eip-3651
[eip3855]: https://eips.ethereum.org/EIPS/eip-3855
[eip3860]: https://eips.ethereum.org/EIPS/eip-3860
[eip4895]: https://eips.ethereum.org/EIPS/eip-4895
[eip6049]: https://eips.ethereum.org/EIPS/eip-6049

[block-validation]: ../rollup-node-p2p.md#block-validation
[payload-attributes]: ../derivation.md#building-individual-payload-attributes
[1559-params]: ../exec-engine.md#1559-parameters
[channel-reading]: ../derivation.md#reading
[deposit-reading]: ../deposits.md#deposit-receipt
[create2deployer]: ../predeploys.md#create2deployer

The Canyon upgrade contains the Shapella upgrade from L1 and some minor protocol fixes.
The Canyon upgrade uses a _L2 block-timestamp_ activation-rule, and is specified in both the
rollup-node (`canyon_time`) and execution engine (`config.canyonTime`). Shanghai time in the
execution engine should be set to the same time as the Canyon time.

## Execution Layer

- Shapella Upgrade
  - [EIP-3651: Warm COINBASE][eip3651]
  - [EIP-3855: PUSH0 instruction][eip3855]
  - [EIP-3860: Limit and meter initcode][eip3860]
  - [EIP-4895: Beacon chain push withdrawals as operations][eip4895]
    - [Withdrawals are prohibited in P2P Blocks][block-validation]
    - [Withdrawals should be set to the empty array with Canyon][payload-attributes]
  - [EIP-6049: Deprecate SELFDESTRUCT][eip6049]
- [Modifies the EIP-1559 Denominator][1559-params]
- [Adds the deposit nonce & deposit nonce version to the deposit receipt hash][deposit-reading]
- [Deploys the create2Deployer to `0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2`][create2deployer]

## Consensus Layer

- [Channel Ordering Fix][channel-reading]
