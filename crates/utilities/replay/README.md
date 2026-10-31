# base-replay

Builder transaction replay records: the `replay-record/v2` JSONL format shared by
recorders and replayers. This crate contains no recorder or storage implementation.

## Wire contract

Each record represents one JSON-RPC call, not an entire JSON-RPC batch. Writers
use `ReplayRecord::to_jsonl_line` for JSONL: one JSON object followed by a newline per call, in
arrival order. `params` retains the original JSON value's bytes, including internal
whitespace and number formatting; whitespace outside that value is not retained.
The v2 envelope is `{"version":2,"record":"<logical record JSON>"}`. Escaping the
complete record as a JSON string preserves literal LF/CR and escaped characters
without compacting params. `ReplayRecord::from_jsonl_line` reverses this framing
and also reads legacy unframed v1 lines. Generic Serde represents logical records,
not the physical JSONL envelope.

Readers must select this schema using the external format identifier
`REPLAY_RECORD_FORMAT` (`replay-record/v2`). Each physical line also identifies its
framing version; unsupported versions are rejected rather than guessed.

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
