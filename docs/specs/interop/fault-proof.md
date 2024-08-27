# Fault Proof

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Cascading dependencies](#cascading-dependencies)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The fault proof design is not complete, but the following docs show high level overviews of the direction:

- [Notes 1](https://oplabs.notion.site/External-Interop-Fault-Dispute-Game-Notes-1537bf9fad054bcfb2245dea88d48d16)
- [Notes 2](https://oplabs.notion.site/External-Interop-Fault-Proofs-spec-WORK-IN-PROGRESS-29cfaae285994870b3fa51254d0391f2)

## Cascading dependencies

Deposits are a special case, synchronous with derivation, at enforced cross-L2 delay.
Thus deposits cannot reference each others events intra-block.

No changes to the dispute game bisection are required. The only changes required are to the fault proof program itself.
The insight is that the fault proof program can be a superset of the state transition function.

## Security Considerations

TODO
