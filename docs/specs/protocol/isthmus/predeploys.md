# Predeploys

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [L1Block](#l1block)
    - [Interface](#interface)
      - [`setIsthmus`](#setisthmus)
  - [FeeVault](#feevault)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

TODO describe operator fee change

### L1Block

#### Interface

##### `setIsthmus`

This function is meant to be called once on the activation block of the holocene network upgrade.
It MUST only be callable by the `DEPOSITOR_ACCOUNT` once. When it is called, it MUST call
call each getter for the network specific config and set the returndata into storage.

### FeeVault

TODO

## Security Considerations
