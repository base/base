---
name: verifying-base
description: "Verifies Base local-devnet transaction inclusion using existing devnet commands and Foundry cast. Use when checking Base changes or demonstrating sequencer transaction inclusion."
---

# Verifying Base

Use Base's existing tools to exercise real behavior and collect evidence, not just inspect code.

1. Read the repository's `AGENTS.md` and the [feature map](references/features/README.md). The map describes supported behavior, how to exercise it, and what counts as success; CLI tools do not parse it.
2. Inspect the checkout's revision and changes. Use a disposable devnet: startup and shutdown delete `.devnet` data, and fixed container names and ports prevent concurrent stacks. Never overwrite someone else's work or stop unrelated services.
3. Follow [local devnet transaction inclusion](references/features/local-devnet-transaction-inclusion.md) from the repository root. Run its real `just`, `docker`, `cast`, and `jq` commands; do not mock RPC responses or substitute a standalone Anvil chain.
4. When verifying a code change, run the relevant existing tests as well. Investigate failures, make warranted fixes within the requested scope, and rerun the checks. A blocked or failed run is not a pass.
5. Report the revision and dirty changes, transaction hash, receipt status, block number/hash, inclusion assertion, and evidence directory. This scenario proves sequencer inclusion only, not finality or whole-product correctness.
6. Shut down only the devnet owned by this run, preserving evidence outside `.devnet`. If asked to keep it running, report ownership and the cleanup command instead.

Update the relevant feature page when commands or behavior change. Keep new coverage explicit and small; no new verification framework is required.
