# base-replay

Builder transaction replay records: the `replay-record/v1` JSON schema shared by
recorders and replayers. This crate contains no recorder or storage implementation.

## Wire contract

Each record represents one JSON-RPC call, not an entire JSON-RPC batch. Writers
serialize records as JSONL: one JSON object followed by a newline per call, in
arrival order. `params` retains the original JSON value's bytes, including internal
whitespace and number formatting; whitespace outside that value is not retained.
Multiline params must not be compacted to force them onto one physical line:
verbatim preservation takes precedence, so such input needs a different framing
contract before it can be written as JSONL.

Readers must select this schema using the external format identifier
`REPLAY_RECORD_FORMAT` (`replay-record/v1`), supplied by the enclosing transport or
dataset metadata. It is not repeated in each record. Readers must reject unsupported
format identifiers before deserializing records; field compatibility alone does not
identify a format version.

- `stream`: `builder_ingress`, or `mempool_ingress` (reserved for future use).
- `received_at_ms`: Unix timestamp in milliseconds at recorder ingress.
- `method`: JSON-RPC method to resend with `params` unchanged.
- `params`: raw JSON-RPC parameters, never decoded into a generic JSON value.
- `batch_id`: optional recorder-assigned `u64`, shared by calls from one incoming
  JSON-RPC batch. Scoped to a capture session, not a JSON-RPC request ID; absent for
  non-batch calls. An integer supports cheap sequential assignment without implying
  global uniqueness. Records retain batch order through their arrival order.
- `source`: remote socket address as observed by the recorder, including port.
- `tx_hash`: optional transaction hash when attributable to one transaction.

Absent optional fields are omitted, not serialized as `null`.
