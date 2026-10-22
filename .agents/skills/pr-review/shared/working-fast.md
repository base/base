## Working from the snapshot

Your prompt holds the description, the changed-file list, the diff, and a source snapshot that the script collected before any model ran. You can also use `Read`, `Grep`, and `Glob` in the reviewed checkout, within a small turn limit. Nobody will answer a question or run a command for you.

- Start from the diff. Use the snapshot to check what the diff does not show: the rest of a changed function, a type definition, a caller in a changed file, the repository guides.
- The snapshot has the changed files, the files earlier review threads point at, `AGENTS.md`, `CLAUDE.md`, and `docs/guides/BLOCK_PRODUCTION_REVIEW.md`, each bounded in size and with line numbers. A file marked unavailable, truncated, or cut off at the snapshot's size limit is incomplete there, and unchanged callers are not in it.
- Use the tools only for a specific question the snapshot cannot answer: an unchanged caller, a definition, a sibling implementation, the missing part of a truncated file. Search narrowly with `Grep` or `Glob`, read only what you need, and stop exploring in time to give your answer; running out of turns fails the stage.
- Do not write as if you opened or searched code you neither were given nor read. When a step you could not check matters, name it and mark the finding `low` confidence.
- Missing context is not evidence of a bug and not evidence that one is absent. Report a problem only when code you were given or read shows it.
- A few findings the code supports are worth more than many guesses. If the riskiest changes check out, report no findings.
- Keep your reasoning short and do not restate the diff.
- Finish with the JSON object required by the system prompt, even when there are no findings. A promise to answer is not a submitted result. There is no repair turn after invalid output.
