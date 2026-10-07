## Finding guide

This guide is appended to the prompt of every agent that reports, merges, or posts findings. It sets one vocabulary and one writing style so the comments read the same whoever wrote them.

### Severity

- `critical`: halts or stalls block production, breaks consensus, or loses or corrupts funds or state.
- `major`: will misbehave in production: wrong results, a panic or hang reachable from real input, a data race, a missing check that lets bad input through.
- `minor`: real but low impact: a missing test for new behavior, needless complexity, an edge case that is unlikely to happen.

Rate by the worst realistic outcome, not by how alarming the code looks.

### Category

Pick the one that names the root cause.

- `block-production`: can halt or stall block production, payload finalization, or validator re-execution; unbounded payloads at an I/O boundary.
- `correctness`: the code does not do what it is meant to do: wrong logic, off-by-one, wrong fork or version handling, arithmetic overflow.
- `concurrency`: locks across `.await`, cancellation safety, races, ordering, shutdown, channels without backpressure.
- `error-handling`: panics from `unwrap` or `expect`, swallowed or misclassified errors, lost error context.
- `safety`: `unsafe`, cryptography, access control, untrusted input, determinism hazards.
- `performance`: avoidable cloning or allocation, I/O or locking on a hot path, unbounded growth.
- `compatibility`: breaks a public API, wire format, on-disk data, or earlier fork behavior.
- `design`: duplicated logic, missed reuse, wrong crate layering, needless abstraction or configuration.
- `tests`: new behavior or a bug fix with no test that would fail without it, or a test that cannot fail.

### Writing

Write for an engineer who has a minute to read this.

- **Title:** at most 80 characters. State the problem, not the topic: "Panics when the batch is empty", not "Empty batch handling".
- **Body:** one to three sentences, plainest words first. Say what is wrong, then when it happens, then what goes wrong. Name the input or state that triggers it. Do not restate the code, hedge, or explain what the change does.
- **Identifiers:** put function, type, and variable names in backticks.
- **Fix:** if you know the fix, give it concretely in one sentence, or a short code block. Do not write "consider" or "you might want to".
- **Evidence:** put a longer trace (call path, scenario, test that would fail) in the evidence field, not the body. Use it for `critical` findings and anything non-obvious. Leave it out otherwise.
- Do not use headings, bold, emoji, or a severity or category prefix in the text. The script adds the header.
- Do not apologize, praise, or add filler.
