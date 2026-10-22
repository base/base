# Organizing Agent Instructions

This guide explains where to put instructions for Claude Code and other agents:
the root `CLAUDE.md`, a separate document, or a task-specific skill. In this
repository, `AGENTS.md` is a symlink to `CLAUDE.md`; edit the target and preserve
the symlink so both entry points provide the same guidance.

## Choose by Scope and Use

| Location | Use it for | Examples |
| --- | --- | --- |
| Root `CLAUDE.md` | Short, durable rules that apply to most changes, plus pointers that tell agents when to load specialized guidance. | Crate conventions, tracing style, test placement. |
| Separate document | Topic-specific contracts, explanations, or review guidance needed only for certain changes. It should also be useful to a human reader. | Cancellation review, block-production review, testing commands. |
| Skill | A reusable, task-triggered procedure that an agent can discover and invoke, especially when it needs commands, scripts, fixtures, or other bundled resources. | A devnet verification procedure or a CI-editing workflow. |

Start with an existing document that owns the topic. Length alone does not make
something a skill, and a checklist does not require one. Use a separate document
when the main need is to understand or check a contract; use a skill when the main
need is to carry out a repeatable task. Do not create both unless they serve
distinct roles: the document explains the contract, and the skill applies it.

## Keep Root Instructions Small and Discoverable

- Put a rule in the root only if agents need it for most work. Keep specialized
  detail out of every PR's context.
- When moving specialized guidance out, leave a short relative link with an
  explicit trigger: "When a change affects X, read and apply Y." A bare link does
  not say when the document is required.
- Define triggers by changed behavior, not broad syntax or file types. For example,
  load cancellation guidance when a change affects interruption or ownership,
  not for every Rust file or every function containing `async`.
- Keep one source of truth. Link to detailed rules rather than copying them into
  root instructions, documents, skills, and review prompts.

## Make Specialized Guidance Reach Its Readers

- Open a document with its topic, scope, and when to read it. Put cross-cutting
  guides in `docs/guides/`; put component documentation next to its code.
- Give a skill a precise description of the task that should trigger it. Follow
  the target agent's supported skill format and discovery locations; do not assume
  every agent discovers the same directories or loads linked files automatically.
- Check the actual consumers when changing guidance: interactive agents and
  automated review workflows may load different files or restrict reads. Ensure
  the conditional pointer can be followed with their available tools and access.
- Keep review output requirements with the workflow that owns them. Specialized
  review guidance must respect that workflow's rules for brevity, actionable
  findings, and silence when there is nothing to report.

## Verify Guidance Changes

- Read the resulting entry point and linked guidance together. Check that the
  trigger is clear, links resolve, and no rules were lost or duplicated during a move.
- Preserve existing rules outside the requested scope and the `AGENTS.md` symlink.
- Check for conflicting instructions in affected review workflows. If a skill is
  added or moved, verify discovery in the intended agent rather than merely checking
  that its file exists.
- Run the relevant Markdown link checks and `git diff --check`. Guidance is not
  mechanical enforcement: use tests or lints for behavior they can actually check.
