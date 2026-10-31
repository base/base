# base-replay-recorder

JSON-RPC ingress request types preserve raw parameters and correlation IDs.
Insert methods attributable to one serialized transaction expose its hash.
Envelope conversion assigns source, arrival timestamp, and a shared batch ID to
records in wire order. Storage preserves multiline params through v2 JSONL framing.
Invalid batch members retain their positions as errors; valid members still record.
Each batch is limited to 1024 members to bound parsing and response expansion.
`ArrivalQueue` admits four requests before body collection, returning HTTP 503
when full. Timestamping and FIFO reservation are atomic; the single writer waits
for earlier bodies and always closes storage when producers finish.
