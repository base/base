# Cancellation and Process Lifetimes

This guide covers cancellation ownership, partial effects, retries, and shutdown
across asynchronous operations, tasks, RPCs, queues, and child processes in any
language. Read it when a change affects those lifetimes or interruption behavior,
not merely because the code is async. Cancellation is a contract of the whole
operation, not just its last await. Apply this review to the requested change and
its producers/consumers, not a repository-wide audit.

## Trace Ownership and Effects

1. Read the operation and its caller. Identify when work is accepted, when effects
   become visible, and when completion is acknowledged. Follow queues, spawned
   tasks, RPCs, and process boundaries to the actual worker and its owner.
2. At each relevant suspension or handoff, trace what survives a losing race/select
   branch, timeout, early return, caller disconnect, task abort, process termination,
   or shutdown. State whether interruption stops execution, requests a cooperative
   stop, transfers ownership, or merely stops waiting.
3. Document who observes completion/errors and releases resources for work that
   can outlive its caller. A token, closed response channel, dropped handle, or kill
   request is not an acknowledged stop. Identify where cancellation is observed;
   verify the library's documented behavior when it is not clear locally.
4. Trace retry and recovery. A new attempt must not unintentionally overlap an old
   one, repeat accepted effects, discard progress, or consume a stale response.
   An unknown outcome is not proof of failure: retain completion, reconcile status,
   or use a justified idempotency mechanism before retrying. Backoff alone does not
   make retries safe.

## Check the Whole Operation

- In repeated races or actor selects, dropping and recreating a losing operation
  must not lose required progress or repeat effects. Otherwise retain the pending
  operation or keep progress in actor-owned state. In Rust, select on `&mut` of a
  future pinned outside the repeated `select!` loop; recreating and pinning it on
  each iteration does not preserve it. Check starvation and responsiveness too.
- Review submission and response handling together. A cancellation-safe receive
  primitive does not make a surrounding send-then-receive operation safe to restart.
  Dropping a response receiver does not retract an enqueued request, including
  one that has executed but has not yet replied.
- Do not infer cancellation from dropping task or process handles. Dropping a
  Tokio `JoinHandle` detaches the task; aborting an async task does not undo effects
  or stop remote work. Started `spawn_blocking` work cannot be aborted. Do not add
  detached tasks just to hide a lost-result bug.
- For child processes, specify termination, bounded waits, reaping, and any
  descendants that must also stop. Aborting a parent need not stop its children
  or blocking work. Verify the API's actual drop behavior.
- Distinguish graceful draining from forced termination. State which accepted work
  may finish, what may be abandoned, and how restart handles incomplete or already
  committed effects. Abrupt process death may bypass cleanup; dropping a local
  future or handle does not imply rollback.
- Intentional abandonment and background work are allowed when ownership and
  effects satisfy the contract. Do not flag every non-cancellation-safe future,
  require cancellation tokens everywhere, or prescribe pinning as a universal fix.

## Exercise the Dangerous Interleaving

- Use existing fixtures and public interfaces to interrupt at applicable boundaries:
  before acceptance, after acceptance while completion is pending, and after an
  effect but before acknowledgement. Use channels/barriers and controlled or paused
  time instead of hoping a sleep reproduces a race.
- Assert completion or acknowledged cancellation, no unintended duplicate effects,
  responsive control requests, and cleanup of owned resources. Retaining a receiver
  alone is not proof of eventual progress. For a bug fix, show the same scenario
  fails without the fix and passes with it.
- Keep regression tests for supported observable contracts, not private helpers or
  incidental call order. In review-only work, report missing coverage without
  modifying files or exercising shared systems.
- For automated reviews, the review workflow's output rules take precedence over
  the reporting guidance below, including brevity and silence when there are no
  actionable findings. The cancellation checks above still apply.
- Otherwise, for each finding, give the file/line, interruption point, surviving work or lost
  state, failing event sequence, user-visible consequence, and smallest fix.
  Separate confirmed defects from assumptions about traffic or library semantics.
  If no defect is found, state the scope examined and untested paths; static
  inspection and lints do not prove cancellation safety.
