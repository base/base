# `base-challenger-e2e`

Behavioural end-to-end test of the challenger.

Forks the target L1 into a pod-local Anvil, hands the fork to a real
`base-challenger` binary running alongside it, and asserts on what that
challenger does — first that it leaves valid games alone, then that it
disputes every classifier path we can stage honestly on that same fork.

This crate currently carries two pieces of that test:

- [`Config`] — the `BASE_CHALLENGER_*` variables are shared with the challenger
  under test, so the driver forks exactly the L1 the challenger is configured
  against. The `CHALLENGER_E2E_*` variables belong to the driver alone.
- [`Scrape`] — a Prometheus text-exposition reader for the challenger's
  `/metrics`. Assertions are made against the challenger's own counters rather
  than against chain state alone.

The driver that consumes them lands in a follow-up PR, which replaces this
README with the full argument for why the test is honest and what each path
asserts.
