---
name: reviewing-cancellation
description: "Checks cancellation ownership and partial effects across asynchronous operations, tasks, RPCs, queues, and child processes. Use when implementing or reviewing cancellation, timeouts, retries, task/process lifetimes, or shutdown in any language."
---

# Reviewing Cancellation

Check whether stopping a caller actually stops its work, who owns any work left
running, and whether retrying can duplicate or lose effects. Apply this to the
requested change and the producers/consumers it depends on, not a repository-wide
audit. For Rust, read [Rust Development for Agents](../../../docs/guides/RUST.md).

## Trace Ownership Before Judging Safety

1. Read the operation's implementation and its caller. Identify when work is
   accepted, when effects become visible, and when completion is acknowledged.
   Follow queues, spawned tasks, RPCs, and process boundaries far enough to find
   the actual worker and its owner.
2. Enumerate relevant interruptions: losing a race/select branch, timeout, early
   return, caller disconnect, task abort, process termination, and shutdown.
   At each reachable suspension or handoff, ask what survives if the caller exits.
3. State the cancellation contract: does interruption stop execution, request a
   cooperative stop, transfer ownership, or merely abandon waiting? Identify who
   observes completion/errors and releases resources. Do not mistake a token,
   closed response channel, dropped handle, or kill request for an acknowledged
   stop. Check the library's documented behavior when it is not clear locally.
4. Trace retry and recovery. Can a new attempt overlap the old one, repeat an
   accepted effect, discard progress, or consume a stale response? An unknown
   outcome is not proof of failure. Require reconciliation, retained completion,
   or a justified idempotency mechanism where retry could duplicate effects.

## Check the Whole Operation

- **Races and actor loops:** if a losing operation is recreated, require that
  dropping and recreating it loses no required progress and repeats no effects.
  Otherwise preserve the pending operation or its state across iterations. In
  Rust, select on a mutable reference to a future pinned outside the repeated
  loop. Review starvation and responsiveness as well as receiver lifetime.
- **Request/reply:** cancellation-safe receive primitives do not make the prior
  submission safe to repeat. Verify what happens to an accepted request if its
  response receiver disappears, including after execution but before reply.
- **Tasks and processes:** check whether handle drop detaches or terminates work.
  Verify cooperative cancellation is observed, necessary joins/waits are bounded,
  and children are reaped. Include descendants and blocking work where applicable;
  aborting the parent need not stop them. Do not add detached tasks just to hide a
  lost-result bug.
- **Shutdown and partial effects:** distinguish graceful draining from forced
  termination. Explain which accepted work may finish, what can be abandoned,
  and how restart handles incomplete or already-committed effects. Abrupt process
  death may bypass cleanup; do not promise rollback merely because a local
  future or handle was dropped.

Intentional abandonment or background work is allowed when its ownership and
effects satisfy the contract. Do not flag every non-cancellation-safe future,
require a cancellation token everywhere, or prescribe pinning as a universal fix.
Backoff limits retry rate; it does not establish cancellation or idempotency.

## Exercise the Dangerous Interleaving

For changed behavior, use existing fixtures and public interfaces to interrupt at
the relevant boundaries: before acceptance, after acceptance while completion is
pending, or after effects but before acknowledgement. Use channels/barriers and
controlled time instead of hoping a sleep reproduces the race.

Assert the required observable outcomes: no unintended duplicate effects, either
completion or acknowledged cancellation, responsive control requests, and cleanup
of owned resources. Retaining a receiver alone is not proof of eventual progress.
For a bug fix, show the same scenario fails without the fix and passes with it.
Keep only regression tests for supported contracts, not assertions about private
helpers or incidental call order. In review-only work, report missing coverage
without modifying files or exercising shared systems.

## Report Evidence, Not a Checklist Verdict

For each finding, give the file/line, interruption point, surviving work or lost
state, failing event sequence, user-visible consequence, and smallest fix. Separate
confirmed defects from assumptions about runtime traffic or library semantics.
If no defect is found, state the scope examined and any untested paths; do not
claim that static inspection or a lint proves cancellation safety.
