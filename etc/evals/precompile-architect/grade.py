#!/usr/bin/env python3
"""Deterministic grader for precompile-architect plans.

A pure function of (case JSON, plan block, judge verdicts). It grades the plan the agent
produced, never the steps it took, and needs no repository access.

Trial pass rule (README, "Eval protocol"):

    pass = no hard fail AND every exact-match check passes AND every critical rubric item is yes

Hard fails, each consensus-relevant:
- touches_frozen: create or modify lists a file the case marks must_not_modify.
- verdict_mismatch: proceed where reject is expected, or the reverse.
- missing_frozen_version: a proceed plan's versions.frozen omits a version the case expects frozen.
  A reject touches nothing, so for rejects the frozen list only feeds partial credit.
- format_failure: no parseable, schema-valid plan block (assigned by the runner).

Exact-match checks, each satisfied by expected or by any grading.alternatives entry:
- approach, activation_fork, versions.create, versions.modify (as sets).
- symmetric_modules must be a superset of expected.

Partial credit only, never decides pass or fail:
- surfaces: fraction of graded flags that match.
- file_recall: share of grading.critical_files the plan touches.
- file_precision: share of touched files that are expected or allowed.
- frozen_coverage: share of must_not_modify files the plan also lists.
- supporting rubric items (yes = 1, no or unknown = 0), scored by the judge.

Usage: python3 grade.py <case.json> <plan.json> [<judge.json>]
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from evallib import load_json, matches_any, normalize_plan  # noqa: E402

EXACT = ("approach", "activation_fork", "versions_create", "versions_modify", "symmetric_modules")
PARTIAL = ("surfaces", "file_recall", "file_precision", "frozen_coverage")


def _answer_keys(case):
    """Expected answer plus alternatives, each as (approach, fork, create, modify)."""
    expected = normalize_plan(case["expected"])
    base = {
        "approach": expected["approach"],
        "activation_fork": expected["activation_fork"],
        "versions": expected["versions"],
    }
    keys = [base]
    for alternative in case["grading"].get("alternatives", []):
        merged = dict(base)
        for field in ("approach", "activation_fork", "versions"):
            if field in alternative:
                merged[field] = alternative[field]
        merged["versions"] = {k: sorted({v.upper() for v in merged["versions"][k]}) for k in ("create", "modify", "frozen")}
        keys.append(merged)
    return keys


def _exact_checks(plan, key, expected_modules):
    return {
        "approach": plan["approach"] == key["approach"],
        "activation_fork": plan["activation_fork"] == key["activation_fork"],
        "versions_create": set(plan["versions"]["create"]) == set(key["versions"]["create"]),
        "versions_modify": set(plan["versions"]["modify"]) == set(key["versions"]["modify"]),
        "symmetric_modules": set(expected_modules) <= set(plan["symmetric_modules"]),
    }


def grade_plan(case, plan, judge=None):
    """Grade one plan. `judge` is judge.py output, or None when the judge has not run yet."""
    plan = normalize_plan(plan)
    expected = normalize_plan(case["expected"])
    grading = case["grading"]
    touched = set(plan["files"]["create"]) | set(plan["files"]["modify"])
    must_not = set(expected["files"]["must_not_modify"])

    hard_fails = []
    frozen_hits = sorted(touched & must_not)
    if frozen_hits:
        hard_fails.append("touches_frozen")
    if plan["verdict"] != expected["verdict"]:
        hard_fails.append("verdict_mismatch")
    missing_frozen = sorted(set(expected["versions"]["frozen"]) - set(plan["versions"]["frozen"]))
    if missing_frozen and plan["verdict"] == "proceed":
        hard_fails.append("missing_frozen_version")

    # Exact match against the best-fitting accepted answer.
    if expected["verdict"] == "reject":
        exact = {name: plan["verdict"] == "reject" for name in EXACT}
        matched_answer = 0 if all(exact.values()) else None
    else:
        candidates = [_exact_checks(plan, key, expected["symmetric_modules"]) for key in _answer_keys(case)]
        best = max(range(len(candidates)), key=lambda i: sum(candidates[i].values()))
        exact = candidates[best]
        matched_answer = best if all(exact.values()) else None

    # Partial credit.
    partial = {}
    graded = grading["graded_surfaces"]
    partial["surfaces"] = 1.0 if not graded else sum(plan["surfaces"][s] == expected["surfaces"][s] for s in graded) / len(graded)
    critical = set(grading["critical_files"])
    partial["file_recall"] = 1.0 if not critical else len(critical & touched) / len(critical)
    allowed = set(expected["files"]["create"]) | set(expected["files"]["modify"])
    contradictory = touched & set(plan["files"]["must_not_modify"])
    stray = sorted(p for p in touched if p in contradictory or (p not in allowed and not matches_any(p, grading["allowed_extra_files"])))
    partial["file_precision"] = 1.0 if not touched else 1 - len(stray) / len(touched)
    file_coverage = 1.0 if not must_not else len(must_not & set(plan["files"]["must_not_modify"])) / len(must_not)
    expected_frozen = set(expected["versions"]["frozen"])
    version_coverage = 1.0 if not expected_frozen else len(expected_frozen & set(plan["versions"]["frozen"])) / len(expected_frozen)
    partial["frozen_coverage"] = (file_coverage + version_coverage) / 2

    # Rubric.
    rubric = {"critical_pass": None, "critical_items": {}, "supporting_score": None, "unknown": 0}
    if judge is not None:
        items = judge.get("items", {})
        for item in case["rubric"]:
            verdict = items.get(item["id"], {}).get("verdict", "missing")
            rubric["unknown"] += verdict == "unknown"
            if item["critical"]:
                rubric["critical_items"][item["id"]] = verdict
        rubric["critical_pass"] = all(v == "yes" for v in rubric["critical_items"].values())
        supporting = [items.get(i["id"], {}).get("verdict") == "yes" for i in case["rubric"] if not i["critical"]]
        rubric["supporting_score"] = sum(supporting) / len(supporting) if supporting else None

    code_pass = not hard_fails and all(exact.values())
    partial_values = list(partial.values()) + ([rubric["supporting_score"]] if rubric["supporting_score"] is not None else [])
    return {
        "pass": code_pass and rubric["critical_pass"] is True,
        "code_pass": code_pass,
        "hard_fails": hard_fails,
        "frozen_files_touched": frozen_hits,
        "missing_frozen_versions": missing_frozen,
        "exact": exact,
        "matched_answer": matched_answer,
        "partial": {k: round(v, 4) for k, v in partial.items()},
        "partial_score": round(sum(partial_values) / len(partial_values), 4),
        "stray_files": stray,
        "rubric": rubric,
    }


def main(argv):
    if len(argv) not in (2, 3):
        raise SystemExit(__doc__)
    judge = load_json(argv[2]) if len(argv) == 3 else None
    print(json.dumps(grade_plan(load_json(argv[0]), load_json(argv[1]), judge), indent=2))


if __name__ == "__main__":
    main(sys.argv[1:])
