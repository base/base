# Cross Domain Messengers

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Message Passing](#message-passing)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Message Passing

The `sendMessage` function MUST revert when Custom Gas Token mode is enabled and `msg.value > 0`.
