# Autonomous agents

Autonomous agents are scheduled workflows that keep one kind of maintenance work moving through a pull request. They are defined in one place, [`registry.json`](registry.json), and run by [`.github/workflows/autonomous-agents.yml`](../workflows/autonomous-agents.yml) through [`etc/scripts/ci/autonomous_agents.py`](../../etc/scripts/ci/autonomous_agents.py).

## Registry

Each entry under `agents` sets:

| Field | Meaning |
|---|---|
| `label` | Label on every pull request the agent opens. The agent counts and manages only pull requests with this label. |
| `branch_prefix` | Prefix of the agent's branches. Must start with `agent/` and end with `-`. |
| `model` | Claude model the agent runs, for example `claude-sonnet-5-5`. |
| `min_open_prs` | Pull requests the agent must keep open while it has work. `0` lets the agent decline: when it decides no update is needed, it opens nothing and closes its open pull request. |
| `max_open_prs` | Hard cap. The agent never opens more and closes any beyond it. |
| `instructions` | The agent's prompt, read by the model at the start of every run. |

`target` names the repository and base branch the pull requests are opened against. The controller validates the registry on every run and the workflow tests validate the committed file, so a bad model name or bounds fail in a pull request rather than in production.

Adding an agent means adding a registry entry, its instructions, and its job in the workflow.

## The docs-index agent

The docs index (`etc/docs-index.toml`, `llms.txt`, `llms-full.txt`) is no longer validated or regenerated before merge. Feature pull requests do not touch it, so they cannot conflict on it. The `docs-index` agent keeps it current instead:

1. On every push to `main` that touches a Markdown doc, hourly, and on manual dispatch, the `plan` job compares the committed index with the docs on `main`.
2. If the index is current, the agent closes its open pull request and deletes the branch.
3. If it is not, the agent refreshes its open pull request or, when none is open, starts a run to create one. At most `max_open_prs` (one, today) stay open.
4. A run checks out `main`, runs `docs-index.py sync`, and gives the model only the new docs and the docs whose summary may be stale. The model edits summaries in `etc/docs-index.toml` and nothing else. When refreshing, summaries an earlier run already wrote for the current content of a doc are reused, so the model sees only what changed since.
5. The controller rejects any other edit, stamps the listed docs, regenerates `llms.txt` and `llms-full.txt`, and runs `docs-index.py check`. It repairs failures with up to three model passes. If the model wrote or changed no summary (every changed doc was re-read and still accurate), the result is not an update: nothing is published, and an open pull request is closed, unless `min_open_prs` requires it to stay.
6. The controller publishes one commit on top of the current `main` through the Git Database API, then checks that GitHub verified the signature. A refresh force-updates the agent branch to that commit. Because the branch is always `main` plus one commit, it does not conflict. If `main` moves during a run, the run exits and the push to `main` that moved it starts another.

Because digests alone never justify a pull request, a doc edit the model judges not to affect its summary leaves the committed digest stale. `plan` keeps reporting work and the model re-reads that doc on every run until a pull request restamps it, for example alongside the next real summary change.

Merge the agent's pull request like any other. Do not push to its branch. Changing a doc after the agent runs makes the pull request stale until the next run refreshes it.

## Configuration

- Secret `LLM_GATEWAY_API_KEY` and variable `LLM_GATEWAY_HOSTNAME`. The model runs on `BaseRunnerGroup`, the only runners that can reach the gateway.
- Actions must be allowed to create pull requests (Settings > Actions > General > Workflow permissions).
- The schedule and `push` triggers start working once the workflow is on `main`. Run it once with `dry_run=true` to read the plan, then with `dry_run=false`.

## Safety boundaries

- The model can read the checkout and edit `etc/docs-index.toml`. It has no shell, no network tools, and no GitHub token; the token is removed from its environment.
- Docs are treated as untrusted data. The controller rejects an edit to any summary the model was not asked about, and any change to a digest or another file.
- The controller and prompts always come from `main`, never from the agent branch.
- The agent opens pull requests against `main` of this repository only. It never merges.

## Validation

```sh
python3 etc/scripts/ci/test_autonomous_agents.py
python3 etc/scripts/local/docs-index.py test
```
