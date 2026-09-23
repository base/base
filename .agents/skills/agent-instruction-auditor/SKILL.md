---
name: agent-instruction-auditor
description: Audit repository-wide instructions and skills for correct ownership and enforceable contracts.
---

# Agent Instruction Auditor

Use this skill to review changes to `AGENTS.md`, `agents/**`, or
`.agents/skills/**`. Report concrete findings and repairs; do not make unrelated
product changes.

## Hierarchy

1. `AGENTS.md` is a concise repository overview for every conversation. It owns
   architecture, style, testing, product direction, and documentation rules. It
   must not prescribe a particular workflow or code-area procedure.
2. Each skill owns only its code-area entry points, flow, canonical owner,
   invariants, failure modes, and focused evidence. It must not duplicate
   repository-wide guidance or imply an unconfigured agent workflow.

## Review procedure

Inspect the diff rather than final files alone. Run:

```sh
python3 etc/scripts/ci/check_skills.py --check
python3 etc/scripts/ci/check_feature_map.py --check
git diff --check
```

Check changed skills for valid frontmatter, working local links, useful and
non-overlapping routing, contradictory requirements, and weakened quality bars.
Report findings with the file and line, the violated hierarchy rule, and a
specific repair. Approve only when the changed guidance remains scoped,
consistent, and enforceable.
