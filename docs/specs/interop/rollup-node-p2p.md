# P2P Networking

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

A node will optimistically accept a block as `unsafe` with only a signature by the sequencer.
To promote this block to a higher level of safety, it must be sure that the initiating messages
exist for all executing messages. A future release may add specific p2p networking components
to decrease the latency of this process. This could look like the sequencer signing and gossiping
sets of executing messages to nodes of remote chains so that they know exactly what initiating
messages to look for. An optimization on this would involve working with commitments to this data
so that less data is sent around via p2p.

## Security Considerations

TODO
