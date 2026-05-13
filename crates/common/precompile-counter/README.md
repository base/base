# base-precompile-counter

Counter precompile for the Base native EVM.

## Interface

```solidity
interface ICounter {
    function increment() external;
    function getCount() external view returns (uint256 count);
}
```

## Address

`0x0000000000000000000000000000000000000900`

## Storage layout

| Slot | Field   | Type    |
|------|---------|---------|
| 0    | `count` | uint256 |
