# base-action-harness

Simulated L1 and L2 actors for protocol integration testing.

The harness provides the building blocks for action tests: discrete,
composable steps that drive protocol actors (L1 miner, batcher, sequencer)
through a scenario and assert on the resulting chain state. No real nodes,
execution engines, or network connections are needed.
