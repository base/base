# L1Withdrawer

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Summary](#summary)
- [Functions](#functions)
  - [`receive`](#receive)
  - [`setMinWithdrawalAmount`](#setminwithdrawalamount)
  - [`setRecipient`](#setrecipient)
  - [`setWithdrawalGasLimit`](#setwithdrawalgaslimit)
- [Events](#events)
  - [`WithdrawalInitiated`](#withdrawalinitiated)
  - [`MinWithdrawalAmountUpdated`](#minwithdrawalamountupdated)
  - [`RecipientUpdated`](#recipientupdated)
  - [`WithdrawalGasLimitUpdated`](#withdrawalgaslimitupdated)
  - [`FundsReceived`](#fundsreceived)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Summary

An optional periphery contract designed to be used as a recipient for a portion of the shares sent
by the `FeeSplitter`. Its sole purpose is to initiate a withdrawal to L1 via `L2CrossDomainMessenger.sendMessage()`
once it has received enough funds. Using the CrossDomainMessenger allows failed withdrawal messages to be replayed,
providing better reliability than direct message passing.

## Functions

### `receive`

Initiates the withdrawal process to L1 if and only if the contract holds funds equal to or above the
`minWithdrawalAmount` threshold.

```solidity
receive() external payable
```

- MUST initiate a withdrawal to the set recipient if and only if the `minWithdrawalAmount` threshold is reached,
  using `L2CrossDomainMessenger.sendMessage()` with the following parameters:
  - `_target`: the configured `recipient` address
  - `_message`: empty bytes (`hex""`) for direct ETH transfer
  - `_minGasLimit`: the configured `withdrawalGasLimit` value
  - `msg.value`: the contract's entire balance
- MUST emit the `FundsReceived` event with the sender, amount received and balance.
- MUST emit the `WithdrawalInitiated` event only if the threshold is reached.

### `setMinWithdrawalAmount`

Updates the minimum withdrawal amount the contract must hold before the withdrawal process can be initiated.

```solidity
function setMinWithdrawalAmount(uint256 _newMinWithdrawalAmount) external
```

- MUST only be callable by `ProxyAdmin.owner()`.
- MUST emit the `MinWithdrawalAmountUpdated` event.
- MUST update the `minWithdrawalAmount` storage variable.

### `setRecipient`

Updates the address that will receive the funds on L1 during the withdrawal process.

```solidity
function setRecipient(address _newRecipient) external
```

- MUST only be callable by `ProxyAdmin.owner()`.
- MUST emit the `RecipientUpdated` event.
- MUST update the `recipient` storage variable.

### `setWithdrawalGasLimit`

Updates the gas limit for the withdrawal on L1.

```solidity
function setWithdrawalGasLimit(uint32 _newWithdrawalGasLimit) external
```

- MUST only be callable by `ProxyAdmin.owner()`.
- MUST emit the `WithdrawalGasLimitUpdated` event.
- MUST update the `withdrawalGasLimit` storage variable.

## Events

### `WithdrawalInitiated`

Emitted when a withdrawal to L1 is initiated.

```solidity
event WithdrawalInitiated(address indexed recipient, uint256 amount)
```

### `MinWithdrawalAmountUpdated`

Emitted when the minimum withdrawal amount before the withdrawal can be initiated is updated.

```solidity
event MinWithdrawalAmountUpdated(uint256 oldMinWithdrawalAmount, uint256 newMinWithdrawalAmount)
```

### `RecipientUpdated`

Emitted when the recipient of the funds on L1 is updated.

```solidity
event RecipientUpdated(address oldRecipient, address newRecipient)
```

### `WithdrawalGasLimitUpdated`

Emitted when the withdrawal gas limit on L1 is updated.

```solidity
event WithdrawalGasLimitUpdated(uint32 oldWithdrawalGasLimit, uint32 newWithdrawalGasLimit)
```

### `FundsReceived`

Emitted whenever funds are received.

```solidity
event FundsReceived(address indexed sender, uint256 amount, uint256 newBalance)
```
