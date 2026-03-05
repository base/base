# Bridges

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

ETH bridging functions MUST revert when Custom Gas Token mode is enabled and the function involves ETH transfers.
This revert behavior is necessary because when a chain operates in Custom Gas Token mode, ETH is no longer the native
asset used for gas fees and transactions. The chain has shifted to using a different native asset entirely.
Allowing ETH transfers could create confusion about which asset serves as the native currency, potentially leading
to user errors and lost funds. Additionally, the custom gas token's supply is managed independently through
dedicated contracts (`NativeAssetLiquidity` and `LiquidityController`), and combining ETH bridging with custom gas
token operations introduces additional complexity to supply management and accounting.
