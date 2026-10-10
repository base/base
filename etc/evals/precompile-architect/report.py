#!/usr/bin/env python3
"""Aggregate a run: metrics, confidence intervals, paired deltas and sanity flags.

- pass^k is primary: the unbiased estimate C(c,k)/C(n,k) of all k trials passing, for c passes
  in n trials. pass@k = 1 - C(n-c,k)/C(n,k) is reported alongside. Here k = n, the run's trials.
- 95% CIs come from a bootstrap that resamples cases (10,000 resamples, fixed seed).
- Treatment vs control is compared per case on pass rate: mean paired delta with a bootstrap CI,
  and an exact two-sided sign test. Claim an effect only when the CI excludes zero.
- Breakdowns by category, polarity and expected verdict/approach.
- Sanity flags: 0% in both arms (suspected broken or ambiguous), 100% in both (saturated), any
  rubric item judged unknown in more than 20% of trials, and unknown-rate gaps between arms.
  Each flag means reading transcripts before reporting results.
- format_failure and infra errors are counted separately; infra errors are excluded from rates.

Usage: python3 report.py results/<run_id>
"""

import collections
import json
import random
import statistics
import sys
from math import comb
from pathlib import Path

ROOT = Path(__file__).resolve().parent
BOOTSTRAP_RESAMPLES = 10_000
UNKNOWN_FLAG_RATE = 0.20


def pass_hat_k(n, c, k):
    return comb(c, k) / comb(n, k) if n >= k else float("nan")


def pass_at_k(n, c, k):
    return 1 - comb(n - c, k) / comb(n, k) if n >= k else float("nan")


def bootstrap_ci(values, rng):
    if not values:
        return (float("nan"), float("nan"))
    means = sorted(statistics.mean(rng.choices(values, k=len(values))) for _ in range(BOOTSTRAP_RESAMPLES))
    return means[int(0.025 * BOOTSTRAP_RESAMPLES)], means[int(0.975 * BOOTSTRAP_RESAMPLES) - 1]


def sign_test(deltas):
    wins, losses = sum(d > 0 for d in deltas), sum(d < 0 for d in deltas)
    n = wins + losses
    if n == 0:
        return 1.0
    tail = sum(comb(n, i) for i in range(min(wins, losses) + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def load(run_dir):
    records = [json.loads(p.read_text()) for p in run_dir.glob("*/*/trial-*.json") if not p.name.endswith(".transcript.json")]
    cases = {json.loads(p.read_text())["id"]: json.loads(p.read_text()) for p in (ROOT / "cases").glob("*.json")}
    return records, cases


def main(argv):
    run_dir = Path(argv[0])
    run = json.loads((run_dir / "run.json").read_text())
    records, cases = load(run_dir)
    rng = random.Random(0)
    arms = [a for a in ("control", "treatment") if any(r["arm"] == a for r in records)]

    per = collections.defaultdict(lambda: {"n": 0, "c": 0, "format": 0, "hard": 0, "infra": 0})
    unknown = collections.defaultdict(lambda: collections.Counter())
    for r in records:
        cell = per[(r["arm"], r["case_id"])]
        if r["infra_error"]:
            cell["infra"] += 1
            continue
        cell["n"] += 1
        cell["c"] += bool(r["grade"]["pass"])
        hard = r["grade"].get("hard_fails", [])
        cell["format"] += "format_failure" in hard
        cell["hard"] += bool(set(hard) - {"format_failure"})
        for item_id, verdict in ((r.get("judge") or {}).get("items") or {}).items():
            unknown[(r["case_id"], item_id)][r["arm"]] += 1
            unknown[(r["case_id"], item_id)][f"{r['arm']}_unknown"] += verdict["verdict"] == "unknown"

    k = run["k"]
    lines = [f"# Run {run['run_id']}", "",
             f"Split `{run['split']}`, k={k}, architect `{run['model']}`, judge `{run['judge_model']}`, "
             f"case set `{run['case_set_sha']}`, Claude Code `{run['claude_code_version']}`.", ""]
    lines += ["| Arm | pass^k | 95% CI | pass@k | Trial pass rate | Hard-fail trials | Format failures | Infra errors |",
              "|---|---|---|---|---|---|---|---|"]
    rates = {}
    for arm in arms:
        cells = {cid: v for (a, cid), v in per.items() if a == arm and v["n"]}
        hat = [pass_hat_k(v["n"], v["c"], min(k, v["n"])) for v in cells.values()]
        at = [pass_at_k(v["n"], v["c"], min(k, v["n"])) for v in cells.values()]
        rates[arm] = {cid: v["c"] / v["n"] for cid, v in cells.items()}
        lo, hi = bootstrap_ci(hat, rng)
        total = sum(v["n"] for v in cells.values())
        lines.append(f"| {arm} | {statistics.mean(hat):.0%} | {lo:.0%} to {hi:.0%} | {statistics.mean(at):.0%} | "
                     f"{sum(v['c'] for v in cells.values()) / max(total, 1):.0%} | {sum(v['hard'] for v in cells.values())} | "
                     f"{sum(v['format'] for v in cells.values())} | {sum(v['infra'] for (a, _), v in list(per.items()) if a == arm)} |")

    if {"control", "treatment"} <= set(arms):
        paired = sorted(set(rates["control"]) & set(rates["treatment"]))
        deltas = [rates["treatment"][c] - rates["control"][c] for c in paired]
        lo, hi = bootstrap_ci(deltas, rng)
        verdict = "CI excludes zero" if lo > 0 or hi < 0 else "no claim: CI includes zero"
        lines += ["", "## Treatment vs control", "",
                  f"Mean paired delta in per-case pass rate: {statistics.mean(deltas):+.0%}, 95% CI {lo:+.0%} to {hi:+.0%} ({verdict}).",
                  f"Treatment better on {sum(d > 0 for d in deltas)} cases, worse on {sum(d < 0 for d in deltas)}, tied on {sum(d == 0 for d in deltas)}. "
                  f"Sign test p = {sign_test(deltas):.3f} over {len(paired)} cases."]

    def breakdown(title, key):
        out = ["", f"## By {title}", "", f"| {title} | " + " | ".join(arms) + " |", "|---|" + "---|" * len(arms)]
        groups = collections.defaultdict(lambda: collections.defaultdict(list))
        for (arm, cid), v in per.items():
            if v["n"] and cid in cases:
                groups[key(cases[cid])][arm].append(v["c"] / v["n"])
        for group in sorted(groups):
            out.append(f"| {group} | " + " | ".join(f"{statistics.mean(groups[group][a]):.0%}" if groups[group][a] else "n/a" for a in arms) + " |")
        return out

    lines += breakdown("category", lambda c: c["category"])
    lines += breakdown("polarity", lambda c: c["polarity"])
    lines += breakdown("decision", lambda c: f"{c['expected']['verdict']}/{c['expected']['approach']}")

    lines += ["", "## Per case", "", "| Case | " + " | ".join(arms) + " |", "|---|" + "---|" * len(arms)]
    flags = []
    for cid in sorted({cid for (_, cid) in per}):
        cells = [per[(a, cid)] for a in arms]
        lines.append(f"| {cid} | " + " | ".join(f"{v['c']}/{v['n']}" + (" HF" if v["hard"] else "") + (" FMT" if v["format"] else "") for v in cells) + " |")
        if all(v["n"] and v["c"] == 0 for v in cells):
            flags.append(f"`{cid}` is 0% in every arm: suspected broken or ambiguous task. Read transcripts.")
        if all(v["n"] and v["c"] == v["n"] for v in cells):
            flags.append(f"`{cid}` is 100% in every arm: saturated.")
    for (cid, item_id), counts in sorted(unknown.items()):
        total = sum(counts[a] for a in arms)
        total_unknown = sum(counts[f"{a}_unknown"] for a in arms)
        if total and total_unknown / total > UNKNOWN_FLAG_RATE:
            flags.append(f"Rubric item `{cid}/{item_id}` is unknown in {total_unknown}/{total} trials: review its wording.")
        if len(arms) == 2 and all(counts[a] for a in arms):
            gap = counts["control_unknown"] / counts["control"] - counts["treatment_unknown"] / counts["treatment"]
            if abs(gap) > UNKNOWN_FLAG_RATE:
                lower = "treatment" if gap > 0 else "control"
                flags.append(f"`{cid}/{item_id}` unknown rate differs by {abs(gap):.0%} between arms; the other arm's plans omit reasoning more than `{lower}`'s.")
    if flags:
        lines += ["", "## Sanity flags", ""] + [f"- {f}" for f in flags]
    lines += ["", "Do not report these numbers until flagged transcripts have been read."]

    report = "\n".join(lines) + "\n"
    (run_dir / "report.md").write_text(report)
    print(report)


if __name__ == "__main__":
    main(sys.argv[1:])
