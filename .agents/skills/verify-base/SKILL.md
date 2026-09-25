---
name: verify-base
description: "Verifies documented Base system behavior using the feature map and existing devnet and test tools. Use when checking Base changes, demonstrating sequencer transaction inclusion, or exercising behavior-verification pages."
---

# Verify Base

Verify real behavior using Base's existing tools.

1. Read the repository's `AGENTS.md` and the [feature map](references/features/README.md).
2. Exercise the relevant feature from the repository root. Investigate failures rather than reporting them as a pass; run relevant tests when checking a code change.
3. Report the tested revision and local changes, what was verified, and the supporting transaction/receipt/block evidence.

Keep the feature map up to date when commands or behavior change.

Automation may propose feature-map updates in draft pull requests (see `docs/runbooks/feature-docs-draft.md`). Generated instructions remain unverified until a person reviews the page and exercises its commands.
