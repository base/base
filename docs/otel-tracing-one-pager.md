# OTel Tracing One Pager

## Goal

This branch adds OpenTelemetry tracing across the Base consensus and execution stack so we can answer a small set of operational questions much more directly:

- How was a block received?
- When did consensus ask execution to build or validate that block?
- When was a transaction received by RPC?
- How far did that transaction make it into execution and payload-building before inclusion or timeout?

The focus is on new OTel traces only. We are not changing logging or metrics semantics.

## What Changed

### 1. Execution RPC and Engine Ingress

Execution now creates explicit server-side spans for inbound RPC and engine requests.

- Auth / engine HTTP requests extract W3C `traceparent` and attach it before the RPC span is created.
- Engine API methods such as `engine_newPayloadV4` and `engine_forkchoiceUpdatedV3` now show up under the same trace as their consensus-side parent when the request comes from `base-consensus`.
- Public RPC calls now have Base-owned spans for `eth_call` and `eth_sendRawTransaction`.
- `eth_call` now breaks down into nested child spans rather than a single opaque RPC span.
- `eth_sendRawTransaction` now shows the decode and submission phases separately.

What this helps track:

- Which inbound RPC triggered execution work.
- Whether a request reached decoding, call preparation, or execution.
- Whether a raw transaction failed during decoding vs pool / sequencer submission.
- Which consensus request led to a particular engine API call on execution.

### 2. Consensus to Execution Trace Propagation

Consensus now injects W3C trace context on all outgoing engine API HTTP calls.

- `traceparent` / `tracestate` are attached to forkchoice, getPayload, and newPayload requests.
- Consensus captures the current OTel context when it enqueues engine work.
- That context is carried through the engine request processor and async task queue so the outgoing HTTP request still belongs to the original trace.

What this helps track:

- `base-consensus` spans and `base-reth-node` spans now join into the same trace for engine traffic.
- We can see parent / child relationships from consensus block-processing to execution block-validation or forkchoice handling.
- Async queue hops inside consensus no longer break the trace tree.

### 3. Base-Owned Payload and Engine Data Context

Base-owned payload wrapper types now carry trace context deeper into execution-owned logic.

- Base payload types preserve the trace context after the engine API boundary.
- That context survives far enough to cover Base-controlled conversion and execution entrypoints.

What this helps track:

- The trace no longer stops at the raw engine RPC boundary.
- We can tie `new_payload_v4` and related execution-side work to the original consensus request.
- Base-controlled payload conversion and submission steps are now visible in the same trace.

### 4. Payload Build / Seal / Gossip Spans

Consensus and execution now emit explicit spans around block-building and block-handling work.

- Execution payload builder emits spans for payload build and transaction execution phases.
- Consensus sequencer flow emits spans for build, seal, conductor, gossip, and engine submission stages.
- Gossip block handling emits spans with block hash / number context.

What this helps track:

- How long payload-building takes.
- Where time is spent while sealing and forwarding a block.
- Whether the expensive part is build, execution, engine submission, or downstream gossip.

### 5. OTLP Initialization and Defaults

The CLI / tracing initialization path was updated so OTLP export is actually reliable in the long-running node runtime.

- OTLP setup happens inside the main Tokio runtime rather than a temporary one.
- Consensus defaults now include the OTLP feature.
- The OTel propagator is configured globally.

What this helps track:

- Traces are exported reliably to Jaeger / Datadog.
- Cross-process W3C context propagation works consistently.

## What We Can Track Now

### Block Receive / Validate Flow

For engine traffic from consensus into execution, we can now trace:

- consensus request creation
- async queue handoff inside consensus
- outgoing engine HTTP request
- inbound execution request span
- Base-owned execution / payload handling steps after ingress

In practice this gives traces shaped roughly like:

```text
base-consensus
  build_on / engine task / seal pipeline
    -> request
      -> new_payload_v4 or fork_choice_updated_v3
        -> Base payload conversion / submission work
```

### Transaction Receive Flow

For `eth_sendRawTransaction`, we can now trace:

- inbound RPC request
- raw transaction decoding
- submission to local pool or forwarding path

In practice this gives traces shaped roughly like:

```text
request
  -> send_raw_transaction
    -> decode_raw_transaction
    -> submit_raw_transaction
      -> send_transaction
```

### eth_call Flow

For `eth_call`, we now trace the main orchestration steps we control:

- request span
- `eth_call`
- `resolve_call_evm_env`
- `load_call_state`
- `build_call_state_db`
- `prepare_call_env`
- `build_call_tx_env`
- `load_call_nonce`
- `convert_call_tx_env`
- `execute_eth_call`

This makes `eth_call` debuggable in Jaeger instead of appearing as one flat RPC span.

## Example Questions This Answers Better

- Did consensus enqueue engine work but lose the trace during an async hop?
- Did execution receive the engine request and start validation?
- Is a slow `eth_call` spending time in state load, env setup, or actual EVM execution?
- Did `eth_sendRawTransaction` fail before decode, during decode, or during pool / sequencer submission?
- When a block is slow to process, is the delay in consensus, the engine RPC boundary, or execution-owned work after ingress?

## Known Gaps

- The regular public JSON-RPC HTTP server still does not extract inbound `traceparent` from all external HTTP requests. We have better spans for those methods, but not full inbound parent propagation on the non-auth server yet.
- Some important work still happens inside upstream `reth` internals such as `reth-engine-tree`, `reth-network`, and `reth-transaction-pool`, where we have limited control without upstream changes.
- `on_new_payload` and `on_forkchoice_updated` still lose some parentage deeper in upstream engine-tree internals because context is dropped across an internal message channel.
- Consensus validator mode is well-covered for engine traffic; sequencer-only paths have more custom spans but still depend on which mode is running.

## Recommended PR Split

This branch can be split into a few reviewable chunks:

1. OTLP initialization and propagator setup.
2. Consensus -> execution trace propagation for engine API calls.
3. Execution ingress spans for engine / RPC requests.
4. Base payload context plumbing across engine-owned payload types.
5. RPC method deep spans for `eth_call` and `eth_sendRawTransaction`.
6. Additional block build / seal / gossip spans.

## Bottom Line

The branch turns the Base stack from a set of disconnected local spans into a traceable CL -> EL flow with meaningful Base-owned operations at the places we actually debug.

The biggest practical win is that we can now follow a block or RPC request across process boundaries and see where time is spent, rather than only seeing isolated spans inside one process at a time.
