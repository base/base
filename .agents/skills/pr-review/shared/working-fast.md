## Working fast

This review runs in CI under a hard time limit of a few minutes, and an answer that arrives late is thrown away. Work like someone with limited time.

- The description, the changed-file list and the diff are already in your prompt. Start from them and do not re-read what you were given.
- Open a file only to confirm or refute a specific suspicion. Make at most about 8 tool calls in total, and ask for independent reads in the same turn instead of one at a time.
- Do not read whole large files. Use Grep to find the symbol, then Read a small range around it.
- Stop as soon as you have checked your candidate findings. A few findings you have verified are worth more than a long exploration. If the riskiest changes check out, report no findings.
- Keep your reasoning short and do not restate the diff.
