# Stage 1 Roles and Requirements

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Permissionless Fault Proofs](#permissionless-fault-proofs)
- [Roles for Stage 1](#roles-for-stage-1)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

This document outlines configuration requirements (including roles and other parameters)
for an implementation of the OP Stack to satsify the Stage 1 decentralization requirements as defined
by
L2 Beat [[1](https://medium.com/l2beat/introducing-stages-a-framework-to-evaluate-rollups-maturity-d290bb22befe), [2](https://medium.com/l2beat/stages-update-security-council-requirements-4c79cea8ef52)].

## Permissionless Fault Proofs

Stage 1 requires a chain to be operating with Permissionless Fault Proofs.

## Roles for Stage 1

Within the context of an OP Stack, the following roles are required for Stage 1:

1. **Upgrade Controller:** Although named for its ability to perform an upgrade, more generally this
   account MUST control any action which has an impact on the determination of a valid L2 state,
   or the custody and settlement of bridged assets.

   This includes upgrading L1 contracts, modifying the implementation of the dispute game, and
   any other safety-critical functions.

2. **Guardian:** This account MUST control any action which may cause a delay in the finalization of
   L2 states and the resulting settlement on L1.

   This includes but is not limited to pausing code paths related to withdrawals.

There may be additional [roles](./configurability.md#admin-roles) in the system, however they MUST
not be able to perform any actions which have an impact on either the validity of L2 states, or the
users ability to exit the system.
