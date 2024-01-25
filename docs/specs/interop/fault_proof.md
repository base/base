# Fault Proof

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Security Considerations](#security-considerations)
  - [Cascading dependencies](#cascading-dependencies)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

TODO

No changes to the dispute game bisection are required. The only changes required are to the fault proof program itself.
The insight is that the fault proof program can be a superset of the state transition function.

## Security Considerations

TODO

### Cascading dependencies

Deposits are a special case, synchronous with derivation, at enforced cross-L2 delay.
Thus deposits cannot reference each others events intra-block.
