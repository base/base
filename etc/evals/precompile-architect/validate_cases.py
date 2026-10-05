#!/usr/bin/env python3
"""Validate precompile-architect eval cases.

Checks run on every case:

- The case matches the schemas, and its id matches its file name.
- fork_state matches chain config at base_commit.
- Files the expected plan creates are absent at base_commit; files it modifies or freezes exist.
- Every critical file is in expected create or modify.
- Every frozen version's logic file is listed in must_not_modify or modify.
- Every case has at least one critical rubric item.
- Every alternative differs from expected in at least one exact-match field.
- The expected plan, used as a reference solution, passes its own grader with full partial credit
  when every rubric item is judged yes.

Historical cases also check:

- base_commit is the parent of reference_commit.
- Expected create and modify lists match the reference diff exactly.
- No must_not_modify file appears in the reference diff.
- Any frozen logic file in the diff changes only inside its test module.

Needs full git history: git fetch --unshallow
Usage: python3 etc/evals/precompile-architect/validate_cases.py [case.json ...]
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from evallib import (  # noqa: E402
    CASES, diff_name_status, exists_at, fork_state, git, is_test_only_change, load_json, schema, validate,
)
from grade import grade_plan  # noqa: E402

PRECOMPILES = "crates/common/precompiles/src"


def frozen_logic_files(case):
    expected = case["expected"]
    paths = []
    for module in expected["symmetric_modules"]:
        for version in expected["versions"]["frozen"]:
            path = f"{PRECOMPILES}/{module}/logic/{version.lower()}.rs"
            if exists_at(case["base_commit"], path):
                paths.append(path)
    return paths


def check_case(case, errors):
    base = case["base_commit"]
    if subprocess_missing(base):
        errors.append(f"base_commit {base} not found; run git fetch --unshallow")
        return
    expected, files = case["expected"], case["expected"]["files"]
    if case["fork_state"] != fork_state(base):
        errors.append("fork_state does not match chain config at base_commit; regenerate cases")
    for path in files["create"]:
        if exists_at(base, path):
            errors.append(f"expected.files.create: {path} already exists at base_commit")
    for path in files["modify"] + files["must_not_modify"]:
        if not exists_at(base, path):
            errors.append(f"expected.files: {path} does not exist at base_commit")
    touched = set(files["create"]) | set(files["modify"])
    for path in case["grading"]["critical_files"]:
        if path not in touched:
            errors.append(f"critical file {path} is not in expected create or modify")
    for path in frozen_logic_files(case):
        if path not in set(files["must_not_modify"]) | set(files["modify"]):
            errors.append(f"frozen logic file {path} is in neither must_not_modify nor modify")
    overlap = set(files["must_not_modify"]) & touched
    if overlap:
        errors.append(f"files both must_not_modify and touched: {sorted(overlap)}")
    if expected["verdict"] == "reject" and (touched or expected["approach"] != "none"):
        errors.append("a reject plan must touch no files and use approach none")
    if not any(item["critical"] for item in case["rubric"]):
        errors.append("case has no critical rubric item")
    for alternative in case["grading"]["alternatives"]:
        if not any(field in alternative for field in ("approach", "activation_fork", "versions")):
            errors.append(f"alternative {alternative['reason']!r} overrides no exact-match field")

    all_yes = {"items": {item["id"]: {"verdict": "yes"} for item in case["rubric"]}}
    result = grade_plan(case, expected, all_yes)
    if not result["pass"] or result["partial_score"] != 1.0:
        errors.append(f"reference plan does not pass its own grader: {result}")

    ref = case["reference_commit"]
    if ref is None:
        return
    parent = git("rev-parse", f"{ref}^").strip()
    if parent != base:
        errors.append(f"base_commit is not the parent of reference_commit (parent is {parent})")
        return
    added, changed = diff_name_status(base, ref)
    for label, listed, actual in (("create", files["create"], added), ("modify", files["modify"], changed)):
        if set(listed) - actual:
            errors.append(f"expected.files.{label} lists files the diff does not touch: {sorted(set(listed) - actual)}")
        if actual - set(listed):
            errors.append(f"diff touches files missing from expected.files.{label}: {sorted(actual - set(listed))}")
    if set(files["must_not_modify"]) & (added | changed):
        errors.append(f"diff touches must_not_modify files: {sorted(set(files['must_not_modify']) & (added | changed))}")
    for path in frozen_logic_files(case):
        if path in changed and not is_test_only_change(base, ref, path) and path not in case["grading"]["critical_files"]:
            note = "listed in modify, so the notes must explain why behavior is unchanged"
            if "unchanged" not in case["notes"] and "signature" not in case["notes"]:
                errors.append(f"frozen logic file {path} changes outside its test module; {note}")


def subprocess_missing(commit):
    return git("cat-file", "-t", commit).strip() != "commit"


def main(argv):
    paths = [Path(p) for p in argv] or sorted(CASES.glob("*.json"))
    case_schema = schema("case.schema.json")
    failed, seen = 0, set()
    for path in paths:
        case = load_json(path)
        errors = validate(case, case_schema, case_schema)
        if not errors:
            if case["id"] != path.stem:
                errors.append(f"id {case['id']!r} does not match file name {path.stem!r}")
            if case["id"] in seen:
                errors.append(f"duplicate id {case['id']!r}")
            seen.add(case["id"])
            check_case(case, errors)
        print(f"{'FAIL' if errors else 'ok':4} {path.name}")
        for error in errors:
            print(f"     - {error}")
        failed += bool(errors)
    print(f"\n{len(paths) - failed}/{len(paths)} cases valid")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
