# `base-challenger-e2e-bin`

Challenger behavioural end-to-end test binary for Base.

Parses CLI arguments and delegates to `base_challenger_e2e::ChallengerE2e::run()`,
which forks the target L1 into a pod-local Anvil, corrupts dispute games the
live challenger has already accepted, and asserts that the challenger sidecar
disputes them. Intended for K8s Job execution.
