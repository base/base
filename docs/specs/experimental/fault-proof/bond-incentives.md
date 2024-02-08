# Bond Incentives

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Moves](#moves)
- [Subgame Resolution](#subgame-resolution)
  - [Leftmost Claim Incentives](#leftmost-claim-incentives)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Bonds is an add-on to the core [Fault Dispute Game](./fault-dispute-game.md). The core game mechanics are
designed to ensure honesty as the best response to winning subgames. By introducing financial incentives,
Bonds makes it worthwhile for honest challengers to participate.
Without the bond reward incentive, the FDG will be too costly for honest players to participate in given the
cost of verifying and making claims.

Implementations may allow the FDG to directly receive bonds, or delegate this responsibility to another entity.
Regardless, there must be a way for the FDG to query and distribute bonds linked to a claim.

Bonds are integrated into the FDG in two areas:

- Moves
- Subgame Resolution

## Moves

Moves must be adequately bonded to be added to the FDG. This document does not specify a
scheme for determining the minimum bond requirement. FDG implementations should define a function
computing the minimum bond requirement with the following signature:

```solidity
function getRequiredBond(Position _movePosition) public pure returns (uint256 requiredBond_)
```

As such, attacking or defending requires a check for the `getRequiredBond()` amount against the bond
attached to the move. To incentivize participation, the minimum bond should cover the cost of a possible
counter to the move being added. Thus, the minimum bond depends only on the position of the move that's added.

## Subgame Resolution

If a subgame root resolves incorrectly, then its bond is distributed to the **leftmost claimant** that countered
it. This creates an incentive to identify the earliest point of disagreement in an execution trace.
The subgame root claimant gets back its bond iff it resolves correctly.

At maximum game depths, where a claimant counters a bonded claim via `step`, the bond is instead distributed
to the account that successfully called `step`.

### Leftmost Claim Incentives

There exists defensive positions that cannot be countered, even if they hold invalid claims. These positions
are located on the same level as honest claims, but situated to its right (i.e. its gindex > honest claim's).

An honest challenger can always successfully dispute any sibling claims not positioned to the right of an honest claim.
The leftmost payoff rule encourages such disputes, ensuring only one claim is leftmost at correct depths.
This claim will be the honest one, and thus bond rewards will be directed exclusively to honest claims.
