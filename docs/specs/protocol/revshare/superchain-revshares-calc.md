# SuperchainRevSharesCalculator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Summary](#summary)
- [Functions](#functions)
  - [`getRecipientsAndAmounts`](#getrecipientsandamounts)
  - [`setShareRecipient`](#setsharerecipient)
  - [`setRemainderRecipient`](#setremainderrecipient)
- [Events](#events)
  - [`ShareRecipientUpdated`](#sharerecipientupdated)
  - [`RemainderRecipientUpdated`](#remainderrecipientupdated)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Summary

A Superchain implementation is provided for the `ISharesCalculator` interface. It pays the greater amount
between 2.5% of gross revenue or 15% of net revenue (gross minus L1 fees) to the configured share recipient.
The second configured recipient receives the full remainder via `FeeSplitter`'s remainder send.

It allows the `ProxyAdmin.owner` to configure the recipient address of the Superchain revenue share and the
recipient of the remainder.

## Functions

### `getRecipientsAndAmounts`

Calculates the share for each of the two recipients based on the following formula:

```solidity
GrossRevenue = Sum of all vault fee revenues
GrossShare = GrossRevenue × 2.5%
NetShare = (GrossRevenue - L1FeeRevenue) × 15%

ShareRecipientAmount = max(GrossShare, NetShare)
RemainderRecipientAmount = GrossRevenue - ShareRecipientAmount
```

```solidity
function getRecipientsAndAmounts(uint256 _sequencerFeeRevenue, uint256 _baseFeeRevenue, uint256 _operatorFeeRevenue, uint256 _l1FeeRevenue) external view returns (ShareInfo[] memory shareInfo)
```

- MUST return the correct partition of shares for each of the `shareRecipient` and `remainderRecipient` addresses
  based on the above formula.

### `setShareRecipient`

Sets the recipient of the shares calculated by the fee sharing formula.

```solidity
function setShareRecipient(address payable _shareRecipient) external
```

- MUST only be callable by `ProxyAdmin.owner`.
- MUST emit `ShareRecipientUpdated`.
- MUST update the `shareRecipient` storage variable.

### `setRemainderRecipient`

Sets the recipient of the remainder of the fees once the shares for the share recipient have been calculated.

```solidity
function setRemainderRecipient(address payable _remainderRecipient) external
```

- MUST only be callable by `ProxyAdmin.owner`.
- MUST emit `RemainderRecipientUpdated`.
- MUST update the `remainderRecipient` storage variable.

## Events

### `ShareRecipientUpdated`

Emitted when the recipient for the calculated share of the fees is updated.

```solidity
event ShareRecipientUpdated(address indexed oldShareRecipient, address indexed newShareRecipient);
```

### `RemainderRecipientUpdated`

Emitted when the recipient for the remainder of the fees is updated.

```solidity
event RemainderRecipientUpdated(address indexed oldRemainderRecipient, address indexed newRemainderRecipient);
```
