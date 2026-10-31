# base-replay-recorder

JSON-RPC ingress request types preserve raw parameters and correlation IDs.
Insert methods attributable to one serialized transaction expose its hash.
Envelope conversion assigns source, arrival timestamp, and a shared batch ID to
records in wire order. Storage preserves multiline params through v2 JSONL framing.
Invalid batch members retain their positions as errors; valid members still record.
