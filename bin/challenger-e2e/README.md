# `base-challenger-e2e-bin`

Challenger behavioural end-to-end test binary for Base.

Parses CLI arguments and delegates to `base_challenger_e2e::ChallengerE2e::run()`,
which forks the target L1 into a pod-local Anvil, releases the challenger
sidecar onto that fork, and asserts the challenger scans it without disputing
anything while every game on it is still valid. Intended for K8s Job execution.

Corrupting games and asserting the dispute paths is layered on top of the same
harness; see the `base-challenger-e2e` crate README for the current scope.
