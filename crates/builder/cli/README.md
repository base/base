# Base Builder CLI

Reusable CLI arguments and config conversion helpers for Base builder nodes.

The builder's optional resource-throttle schedule is loaded at startup with
`--builder.resource-throttle-schedule=<path>`. It computes builder-local resource units from
metering results; it does not alter protocol gas prices or transaction gas limits. The schedule can
also be replaced atomically through the JWT-authenticated `base_replaceResourceThrottleSchedule`
RPC. Legacy execution-time and state-root-gas limits remain available for rollout and are evaluated
alongside resource-throttle limits when explicitly configured.

For a safe rollout, start with `--builder.execution-metering-mode=off`, switch to `dry-run` to
observe would-reject metrics, and use `enforce` only after the schedule and limits are verified.
Resource-throttle updates take effect on the next payload build. Metering nodes collect only the
operation names they were configured for, so configure and restart those nodes before adding new
opcode, precompile, or pseudo-opcode rules to a schedule.
