# Pectra Blob Schedule (Sepolia)

## Activation Timestamps

| Network | Activation timestamp |
| --- | --- |
| `mainnet` | Not activated |
| `sepolia` | `1742486400` (2025-03-20 16:00:00 UTC) |

The Pectra Blob Schedule hardfork is an optional hardfork which delays the adoption of the
Prague blob base fee update fraction until the specified time. Until that time, the Cancun
update fraction from the previous fork is retained.

Note that the activation logic for this upgrade is different to most other upgrades.
Usually, specific behavior is activated at the _hard fork timestamp_, if it is not nil,
and continues until overridden by another hardfork.
Here, specific behavior is activated for all times up to the hard fork timestamp,
if it is not nil, and then _deactivated_ at the hard fork timestamp.

## Consensus Layer

- [Derivation](derivation.md)
