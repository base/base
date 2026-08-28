# Base Transaction Ingress

Private streaming transaction ingress for Base mempool nodes.

The service accepts EIP-2718 encoded transactions over a persistent bidirectional gRPC stream and
submits each transaction through the same admission path as `eth_sendRawTransaction`. Responses may
arrive out of order and are correlated with the request ID supplied by the client.

Set `--tx-ingress.addr <IP:PORT>` on a Base execution node to enable the service. Request IDs are
scoped to one live stream and require no durable state. The server does not limit connections or
concurrent admissions; every received transaction is submitted independently.
