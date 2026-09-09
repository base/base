# Execution network wire protocol

ETH/SNAP messages, RLP encodings, and RLPx transport for Base execution networking.

Message types support `no_std`. Enable `transport` for encrypted framing, handshakes,
capability negotiation, and asynchronous protocol streams. Chain identity and fork IDs
are supplied by network configuration. `serde` and `arbitrary` add serialization and
property-test support.
