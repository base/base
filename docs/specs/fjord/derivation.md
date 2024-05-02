# Fjord L2 Chain Derivation Changes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Protocol Parameter Changes](#protocol-parameter-changes)
  - [Constant Maximum Sequencer Drift](#constant-maximum-sequencer-drift)
    - [Rationale](#rationale)
    - [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

# Protocol Parameter Changes

The following table gives an overview of the changes in parameters.

| Parameter | Pre-Fjord (default) value | Fjord value | Notes |
| --------- | ------------------------- | ----------- | ----- |
| `max_sequencer_drift` | 600 | 1800 | Was a protocol parameter since Bedrock. Now becomes a constant. |

## Constant Maximum Sequencer Drift

With Fjord, the `max_sequencer_drift` parameter becomes a constant of value `1800` _seconds_,
translating to a fixed maximum sequencer drift of 30 minutes.

Before Fjord, this was a chain parameter that was set once at chain creation, with a default
value of `600` seconds, i.e., 10 minutes. Most chains use this value currently.

### Rationale

Discussions amongst chain operators came to the unilateral conclusion that a larger value than the
current default would be easier to work with. If a sequencer's L1 connection breaks, this drift
value determines how long it can still produce blocks without violating the timestamp drift
derivation rules.

It was furthermore agreed that configurability after this increase is not important. So it is being
made a constant. An alternative idea that is being considered for a future hardfork is to make this
an L1-configurable protocol parameter via the `SystemConfig` update mechanism.

### Security Considerations

The rules around the activation time are deliberately being kept simple, so no other logic needs to
be applied other than to change the parameter to a constant. The first Fjord block would in theory
accept older L1-origin timestamps than its predecessor. However, since the L1 origin timestamp must
also increase, the only noteworthy scenario that can happen is that the first few Fjord blocks will
be in the same epoch as the the last pre-Fjord blocks, even if these blocks would not be allowed to
have these L1-origin timestamps according to pre-Fjord rules. So the same L1 timestamp would be
shared within a pre- and post-Fjord mixed epoch. This is considered a feature and is not considered
a security issue.
