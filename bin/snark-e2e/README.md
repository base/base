# `base-snark-e2e-bin`

SNARK PLONK end-to-end prover verification binary for Base.

Parses CLI arguments and delegates to the `base-snark-e2e` library, which
submits a one-block SNARK prove request, polls until completion, and
cryptographically verifies the receipt. Intended for K8s CronJob execution.
