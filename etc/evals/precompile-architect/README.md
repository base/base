# Precompile architect evals

Eval suite for the precompile architect skill. The architect turns a ticket into a plan for changing a native precompile without breaking execution consensus. The suite measures whether the skill does that better than the same model without it. The design rationale is in [TDD.md](TDD.md).

## Layout

| Path | Purpose |
|---|---|
| `generate_cases.py` | Every case definition and hand label. It writes `cases/`. Edit cases here, not in the JSON. |
| `cases/` | One generated JSON task per case. |
| `schemas/` | The plan the architect must produce, and the case format. |
| `prompts/` | The harness prompt, the output contract and the judge prompt. |
| `validate_cases.py` | Checks cases against the schemas, chain config and the merged diff. |
| `grade.py` | Grader. A pure function of the case, the plan block and the judge verdicts. |
| `judge.py` | LLM judge. One isolated call per rubric item, answering yes, no or unknown. |
| `run.py` | Runner. One sandboxed `claude -p` session per case, arm and trial. |
| `report.py` | Aggregator. pass^k, confidence intervals, paired deltas and sanity flags. |
| `calibrate.py` | Judge calibration against human labels. |
| `test_grader.py` | Grader self-tests. |

## Cases

There are 37 cases. 31 are historical: the expected files come from the merged commit. 6 are synthetic and start at a pinned recent commit. 23 cases are `dev` and 14 are `holdout`.

Cases span every upgrade era, because whether code is frozen depends on what is live at `base_commit`.

| Era at `base_commit` | What a correct plan does |
|---|---|
| Pre-Beryl | Edits shared code in place. Nothing has executed. |
| Beryl live on zeronet only | Edits in place. The ticket says zeronet is not preserved. |
| Beryl live | Freezes V1. Scaffolds or edits an unshipped V2. Edits live V1 only to restore what networks executed. |
| Cobalt scheduled | Freezes V2 as well. Denim work goes into V3. |
| Cobalt live | Rejects changes to V1 or V2. New behavior goes into V4 at Everest. |

Each category has positive cases, where its move is right, and negative cases, where it is a near miss.

## Plan decisions

| Field | Values |
|---|---|
| `verdict` | `proceed`, or `reject` when doing the request as asked would change how produced blocks execute. |
| `approach` | `edit_in_place`, `new_logic_version`, `fork_gate`, or `none` for a reject. |
| `activation_fork` | `Beryl`, `Cobalt`, `Denim`, `Everest` or null. |
| `versions` | Versions created, versions whose behavior changes, and versions that must stay byte-for-byte unchanged. |

### Labelling convention

These rules apply to every case. Per-case notes explain only exceptions.

- **`activation_fork`** is the earliest fork whose execution differs from the code at `base_commit`. Forks defined in code but not yet scheduled count. It is null only when execution at every fork is unchanged, such as a pure refactor or a test-only change. So a pre-Beryl fix is labelled `Beryl`, and a restore of live V1 is labelled with the fork V1 serves.
- **`surfaces`** compares execution at `activation_fork` before and after the change.
  - `revert_bytes`: a call starts reverting, stops reverting, or reverts with different bytes.
  - `gas`: a call with the same outcome is charged differently, including work done before a revert.
  - `storage`: the layout changes, or a call that succeeds before and after writes different slots.
  - `abi`: selectors, or error or event definitions, change.
  - `events`: a call that succeeds before and after emits different logs.

  A call that starts or stops reverting sets only `revert_bytes`.
- **`versions.frozen`** lists every existing version of a changed module that must not change.
- **Alternatives.** When experts would accept more than one answer, the case lists it under `grading.alternatives`. One example is a fork gate where an in-place edit suffices.

Until a second person reviews the labels, `surfaces` is partial credit only.

## Validate

```sh
git fetch --unshallow
python3 etc/evals/precompile-architect/generate_cases.py
python3 etc/evals/precompile-architect/validate_cases.py
python3 etc/evals/precompile-architect/test_grader.py
```

The validator requires every reference solution to pass its own grader, every case to have at least one critical rubric item, and every historical case's file lists to match the merged diff.

## Eval protocol

```
cases/*.json (frozen, versioned by case-set SHA)
       |
       v
  RUNNER ---------> GRADER -----------> AGGREGATOR --------> ERROR ANALYSIS
  case x arm x      code grader +       pass^k, pass@k,      read flagged
  trial, fresh      blind LLM judge     bootstrap CI,        transcripts
  sandbox each                          paired delta         |
       ^                                                     |
       +---- edit the skill (dev cases only) <---------------+
             edit a case only on proven ambiguity, logged
```

### Arms and harness prompt

- **A paired design.** Every case runs in `treatment`, with the skill, and `control`, the same model without it.
- **One harness prompt for both arms.** It states the task, the upgrade table and the output contract, and includes `schemas/plan.schema.json` verbatim. The skill is never the only source of the output format, so the measured effect is not a format effect.
- **Format failures.** A trial with no parseable, schema-valid plan block is a `format_failure`. It counts as a fail and is reported apart from reasoning failures. Omitted empty fields are repaired and logged as format repairs.

### Isolation

Each trial runs in a fresh sandbox.

- **Repository.** An empty repository fetches `base_commit` with no depth limit and checks it out detached. Ancestors stay available for `git log` and `git blame`. Descendants, including the reference commit, are never fetched. The runner refuses any `base_commit` that already contains `etc/evals/`.
- **Config.** A throwaway `CLAUDE_CONFIG_DIR` per trial loads no user memory, skills or settings. The treatment arm installs only the skill under test there.
- **Tools.** Read, Grep, Glob, Bash and Skill are allowed. Edit, Write, web tools and sub-agents are not. `--strict-mcp-config` with an empty config loads no MCP servers.
- **Network.** Bash runs in Claude Code's sandbox with an empty network allowlist and no unsandboxed retry. `gh`, `curl` and `git fetch` fail. Only the `claude` process reaches the model endpoint.
- **Organization hooks.** Machine-wide managed settings still load. They affect both arms equally.

### Trials and metrics

| Split | Trials per case per arm |
|---|---|
| `dev` | 3 |
| `holdout` | 5 |

A full run is 278 sessions:

```
(23 dev x 3 + 14 holdout x 5) x 2 arms = 278 sessions
```

- **pass^k is primary.** Consensus safety needs reliability. It is estimated per case as C(c,k)/C(n,k) for c passes in n trials. pass@k is reported alongside.
- **Breakdowns.** The report gives per-case, per-category, per-polarity and per-decision results.
- **Significance.** 95% confidence intervals come from a bootstrap over cases. Treatment and control are compared per case: a mean paired delta with a bootstrap interval, plus an exact sign test. Claim an effect only when the interval on the paired delta excludes zero.
- **Sanity flags.** A case at 0% in both arms is suspected broken or ambiguous. A case at 100% in both arms is saturated. Both require reading transcripts before results are reported.

### Grading

A trial passes when all three conditions hold:

1. **No hard fail.**
   - The plan lists a must-not-modify file under create or modify.
   - The verdict is wrong.
   - A proceed plan omits an expected frozen version.
2. **Every exact-match check passes.** `approach`, `activation_fork`, `versions.create` and `versions.modify` must match the expected answer or one listed alternative. `symmetric_modules` must be a superset of the expected list.
3. **Every critical rubric item is `yes`.**

Partial credit is reported but never decides a pass:

- `surfaces`: the share of graded flags that match.
- File recall on the case's critical files, and file precision against expected and allowed paths. Incidental `lib.rs`, `mod.rs`, test files and fakes are always allowed.
- Frozen coverage: the share of must-not-modify files and frozen versions the plan lists.
- Supporting rubric items: `yes` scores 1, while `no` and `unknown` score 0.

### LLM judge

- **A fixed, separate model.** `claude-opus-5` at temperature 0 is called directly on the Messages API, separate from the architect.
- **Blind to arm.** It sees the ticket, upgrade table, expected plan, grader notes, one rubric criterion and the architect's full final reply. It never learns which arm produced the plan.
- **Output.** One verdict per item, `yes`, `no` or `unknown`, with a quote and a one-sentence reason.
- **The meaning of `unknown`.** It means the plan never addresses the item. It is not a hedge: the judge must answer `yes` or `no` whenever the plan takes a position.
- **Unknown on a critical item fails the trial.** A consensus-safety argument the plan never makes is a missing argument. Unknowns are counted separately and never folded into `no`.
- **Errors.** Transport errors and malformed replies are retried three times. A critical item still in error blocks a pass.
- **Unknown as a signal.**
  - An item judged `unknown` in more than 20% of trials across both arms is flagged as ambiguously worded.
  - A gap of more than 20 points between the arms' unknown rates is flagged too, because it usually means one arm's plans omit their reasoning.

### Judge calibration

Calibration is required before any scored run.

- **Label a set by hand.** Label about 30 case and plan pairs with the same three values. Span real plans, plans with injected flaws, borderline plans, and plans that leave an item out.
- **Agreement target.** Require at least 90% per-item agreement between judge and human, counting all three values.
- **Track unknown fallback.** Separately count how often the judge says `unknown` where the human decided. A judge that falls back to `unknown` can look accurate while quietly failing plans.
- **Re-calibrate.** Do it again whenever the judge model or a rubric changes.

```sh
python3 etc/evals/precompile-architect/calibrate.py sample etc/evals/precompile-architect/results/<run_id>
python3 etc/evals/precompile-architect/calibrate.py score etc/evals/precompile-architect/calibration/<run_id>.jsonl
```

### Holdout and case hygiene

- **Holdout runs only at declared milestones.** The runner requires `--milestone` for holdout and appends date, run, skill SHA and case-set SHA to `holdout_log.jsonl`.
- **Authors don't read holdout cases.** Sessions authoring the skill must not open holdout case files.
- **Cases change only on evidence.** Change them through `generate_cases.py` and `validate_cases.py`, and only when a label is wrong or a task is ambiguous. Never change a case because an arm failed it. Log each change in the commit message.

### Result record

Each trial writes `results/<run_id>/<case_id>/<arm>/trial-<n>.json`, with the raw transcript beside it. A record holds:

- the case id, arm, trial number and split
- the architect model ids, judge model id and Claude Code version
- the skill SHA and case-set SHA
- the timestamp and transcript path
- the parsed plan block, format repairs and grader output
- per-item judge verdicts with reasons
- cost, turns and elapsed time

`run.json` records the run configuration, and `report.md` the aggregate.

### Running

```sh
python3 etc/evals/precompile-architect/run.py --run-id dev-01 --split dev \
  --model <architect-model> --skill path/to/precompile-architect

python3 etc/evals/precompile-architect/run.py --run-id m1-holdout --split holdout \
  --milestone "skill v1" --model <architect-model> --skill path/to/precompile-architect
```

### Follow-ups

- Build the calibration set and record its agreement before the first scored run.
- Get a second reviewer for case labels, then promote `surfaces` to an exact-match check.
- Add an end-to-end layer that passes plans to a fixed implementer and runs the golden suites.
- Reuse the harness to evaluate the precompile reviewer on plans with seeded flaws.
