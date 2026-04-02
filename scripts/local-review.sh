#!/usr/bin/env bash
# Run a local code review that mirrors the GitHub Claude Code Review bot.
# Usage: ./scripts/local-review.sh [base-branch]
#   base-branch defaults to "main"
set -euo pipefail

BASE="${1:-main}"
DIFF=$(git diff "$BASE"...HEAD)

if [ -z "$DIFF" ]; then
  echo "No diff between $BASE and HEAD — nothing to review."
  exit 0
fi

PROMPT=$(cat <<'REVIEW_PROMPT'
You are reviewing a code diff for Base Reth Node — a Rust Ethereum L2 node built on Reth.

CODEBASE:
- 4 core crate groups: crates/{client, builder, consensus, shared}
- devnet/: system and E2E test infrastructure for Base node
- Error handling: thiserror enums with From impls
- Async: tokio, async-trait, arc-swap for lock-free config
- CI already enforces: clippy (52 rules, -D warnings), rustfmt (2024 edition), cargo-deny, cargo-udeps

REVIEW GUIDELINES:
- Review like a senior Rust engineer on the team
- Focus on correctness, safety, and idiomatic Rust
- DO NOT comment on formatting or style — clippy and rustfmt handle that
- DO NOT post praise, "looks good", or filler comments. If everything looks fine, say "No issues found."

WHAT TO LOOK FOR:
- Error handling: .unwrap()/.expect() in non-test code, discarded error context (.map_err(|_| ...)), missing From impls
- Memory & performance: unnecessary .clone() on large types in hot paths, unbounded collections from external input
- Concurrency: locks held across .await, channel misuse (no backpressure, unhandled closed channels), cancellation safety in tokio::select!
- Safety: unsafe without // SAFETY comments, unchecked arithmetic on financial/gas values, HashMap in determinism-sensitive code
- API design: missing #[must_use] on Result-returning public methods, breaking changes to public interfaces
- Architecture: dependency direction violations (shared depending on client/builder), tight coupling between crate layers
- Rust idioms: &String instead of &str, &Vec<T> instead of &[T], manual implementations of standard traits, needless lifetime annotations

OUTPUT FORMAT:
For each finding, report:
  file:line — description of the issue and suggested fix

Group findings under "New findings" vs "Nits" (minor style/idiom suggestions).
If there are no findings, say "No issues found."

Here is the diff to review:
REVIEW_PROMPT
)

echo "$PROMPT"$'\n\n'"$DIFF" | claude -p --model claude-opus-4-6 --allowedTools "Read,Bash(git diff:*),Bash(git log:*),Bash(git show:*)"
