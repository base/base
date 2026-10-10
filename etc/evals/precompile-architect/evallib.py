"""Shared helpers for the precompile-architect eval: git access, schema checks, plan parsing."""

import datetime
import fnmatch
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SCHEMAS = ROOT / "schemas"
CASES = ROOT / "cases"
PROMPTS = ROOT / "prompts"
REPO = Path(subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, cwd=ROOT).stdout.strip())

FORKS = ("Beryl", "Cobalt", "Denim", "Everest")
NETWORKS = ("mainnet", "sepolia", "zeronet")
SURFACES = ("gas", "revert_bytes", "storage", "abi", "events")

# Files a plan may touch without a precision penalty in any case: plumbing and tests.
DEFAULT_ALLOWED = (
    "crates/common/precompile*/src/lib.rs",
    "crates/common/precompile*/src/**/mod.rs",
    "crates/common/precompile*/Cargo.toml",
    "crates/common/precompiles/tests/**",
    "crates/common/precompiles/src/common/test_utils.rs",
    "crates/common/precompiles/src/common/ops/*.rs",
)


def git(*args, check=False):
    result = subprocess.run(["git", *args], capture_output=True, text=True, cwd=REPO)
    if check and result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def full_sha(rev):
    return git("rev-parse", rev, check=True).strip()


def exists_at(commit, path):
    return subprocess.run(["git", "cat-file", "-e", f"{commit}:{path}"], capture_output=True, cwd=REPO).returncode == 0


def diff_name_status(base, ref):
    """Return (added, changed) repo paths between two commits, ignoring Cargo.lock."""
    added, changed = set(), set()
    for line in git("diff", "--name-status", "--no-renames", base, ref, check=True).splitlines():
        status, path = line.split("\t")
        if path == "Cargo.lock":
            continue
        (added if status == "A" else changed).add(path)
    return added, changed


def _test_module_line(text):
    match = re.search(r"^#\[cfg\(test\)\]\s*\n\s*mod tests", text, re.M)
    return text[: match.start()].count("\n") + 1 if match else None


def is_test_only_change(base, ref, path):
    """True when every hunk of a modified Rust file falls inside its `#[cfg(test)] mod tests`."""
    if not path.endswith(".rs") or "/tests/" in path:
        return "/tests/" in path
    old_start = _test_module_line(git("show", f"{base}:{path}"))
    new_start = _test_module_line(git("show", f"{ref}:{path}"))
    diff = git("diff", "-U0", base, ref, "--", path)
    for hunk in re.finditer(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", diff, re.M):
        old_line, old_len = int(hunk[1]), int(hunk[2] or 1)
        new_line, new_len = int(hunk[3]), int(hunk[4] or 1)
        if old_len and (old_start is None or old_line < old_start):
            return False
        if new_len and (new_start is None or new_line < new_start):
            return False
    return True


def fork_state(base):
    """Live and scheduled upgrades per network, read from chain config at `base`."""
    timestamp = int(git("log", "-1", "--format=%ct", base, check=True))
    config = git("show", f"{base}:crates/common/chains/src/config.rs", check=True)
    state = {"as_of": datetime.datetime.fromtimestamp(timestamp, datetime.UTC).strftime("%Y-%m-%d")}
    for network in NETWORKS:
        block = re.search(rf"^const {network.upper()}: ChainConfig = ChainConfig \{{(.*?)^\}};", config, re.S | re.M)
        live, scheduled = [], []
        for fork in FORKS:
            found = re.search(rf"{fork.lower()}_timestamp: Some\(([0-9_]+)\)", block.group(1)) if block else None
            if found:
                (live if int(found.group(1).replace("_", "")) <= timestamp else scheduled).append(fork)
        state[network] = {"live": live, "scheduled": scheduled}
    return state


def matches_any(path, patterns):
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)


def case_set_sha():
    """Hash of every case file, so results record exactly which case set produced them."""
    import hashlib
    digest = hashlib.sha256()
    for path in sorted(CASES.glob("*.json")):
        digest.update(path.name.encode())
        digest.update(path.read_bytes())
    return digest.hexdigest()[:16]


def load_json(path):
    return json.loads(Path(path).read_text())


def load_cases(ids=None, split=None):
    cases = [load_json(path) for path in sorted(CASES.glob("*.json"))]
    if ids:
        wanted = set(ids)
        cases = [case for case in cases if case["id"] in wanted]
        missing = wanted - {case["id"] for case in cases}
        if missing:
            raise SystemExit(f"unknown case ids: {sorted(missing)}")
    if split and split != "all":
        cases = [case for case in cases if case["split"] == split]
    return cases


# --- JSON Schema subset validator -------------------------------------------------------------

_SCHEMA_CACHE = {}


def schema(name):
    if name not in _SCHEMA_CACHE:
        _SCHEMA_CACHE[name] = load_json(SCHEMAS / name)
    return _SCHEMA_CACHE[name]


def _type_ok(value, expected):
    checks = {
        "object": lambda v: isinstance(v, dict),
        "array": lambda v: isinstance(v, list),
        "string": lambda v: isinstance(v, str),
        "boolean": lambda v: isinstance(v, bool),
        "null": lambda v: v is None,
    }
    return any(checks[t](value) for t in (expected if isinstance(expected, list) else [expected]))


def validate(value, node, root, path="$", errors=None):
    """Validate against the JSON Schema subset these schemas use. Returns a list of errors."""
    errors = [] if errors is None else errors
    if "$ref" in node:
        ref = node["$ref"]
        if ref.startswith("#/$defs/"):
            return validate(value, root["$defs"][ref.split("/")[-1]], root, path, errors)
        name, _, pointer = ref.partition("#")
        target_root = schema(name)
        target = target_root
        for part in filter(None, pointer.split("/")):
            target = target[part]
        return validate(value, target, target_root, path, errors)
    if "type" in node and not _type_ok(value, node["type"]):
        errors.append(f"{path}: expected {node['type']}, got {type(value).__name__}")
        return errors
    if "enum" in node and value not in node["enum"]:
        errors.append(f"{path}: {value!r} not in {node['enum']}")
    if "pattern" in node and isinstance(value, str) and not re.search(node["pattern"], value):
        errors.append(f"{path}: {value!r} does not match {node['pattern']}")
    if isinstance(value, dict):
        for key in node.get("required", []):
            if key not in value:
                errors.append(f"{path}: missing required field {key!r}")
        properties = node.get("properties", {})
        if node.get("additionalProperties") is False:
            errors.extend(f"{path}: unexpected field {key!r}" for key in value if key not in properties)
        for key, sub in properties.items():
            if key in value:
                validate(value[key], sub, root, f"{path}.{key}", errors)
    if isinstance(value, list) and "items" in node:
        for index, item in enumerate(value):
            validate(item, node["items"], root, f"{path}[{index}]", errors)
    return errors


# --- Plan extraction and normalization --------------------------------------------------------

_FENCE = re.compile(r"```(?:json)?[ \t]*\n(.*?)```", re.S)


def normalize_path(path):
    path = path.strip().strip("`")
    path = re.sub(r"^\./", "", path)
    marker = path.find("crates/")
    if path.startswith("/") and marker != -1:
        path = path[marker:]
    return path


def normalize_plan(plan):
    """Canonicalize version names and paths so formatting differences never cost points."""
    def versions(items):
        return sorted({str(item).strip().upper() for item in items})

    plan = json.loads(json.dumps(plan))
    for key in ("create", "modify", "frozen"):
        plan["versions"][key] = versions(plan["versions"][key])
    for key in ("create", "modify", "must_not_modify"):
        plan["files"][key] = sorted({normalize_path(item) for item in plan["files"][key]})
    return plan


def repair_plan(raw):
    """Fill omitted empty fields and relocate misplaced keys. Returns (plan, repairs).

    Formatting slips such as an omitted empty list must not cost correctness points; they are
    reported as repairs so format compliance is measured separately. Only fields whose absence
    is unambiguous are filled: empty lists, false surfaces and a null fork. A missing or invalid
    verdict is never repaired.
    """
    plan = json.loads(json.dumps(raw))
    repairs = []
    for misplaced in ("create", "modify", "must_not_modify"):
        if misplaced in plan and isinstance(plan[misplaced], list):
            plan.setdefault("files", {})
            if isinstance(plan["files"], dict) and misplaced not in plan["files"]:
                plan["files"][misplaced] = plan.pop(misplaced)
                repairs.append(f"moved top-level {misplaced} into files")
    for group, keys in (("versions", ("create", "modify", "frozen")), ("files", ("create", "modify", "must_not_modify"))):
        if not isinstance(plan.get(group), dict):
            if plan.get(group) not in (None, {}, []):
                continue
            plan[group] = {}
            repairs.append(f"filled missing {group}")
        for key in keys:
            if key not in plan[group]:
                plan[group][key] = []
                repairs.append(f"filled {group}.{key} with []")
    if "symmetric_modules" not in plan:
        plan["symmetric_modules"] = []
        repairs.append("filled symmetric_modules with []")
    surfaces = plan.get("surfaces")
    if surfaces is None or isinstance(surfaces, dict):
        plan["surfaces"] = surfaces = surfaces or {}
        for key in SURFACES:
            if key not in surfaces:
                surfaces[key] = False
                repairs.append(f"filled surfaces.{key} with false")
    if "activation_fork" not in plan:
        plan["activation_fork"] = None
        repairs.append("filled activation_fork with null")
    tests = plan.get("tests")
    if tests is None or isinstance(tests, dict):
        plan["tests"] = tests = tests or {}
        tests.setdefault("new", [])
        if "existing_goldens_unchanged" not in tests:
            tests["existing_goldens_unchanged"] = True
            repairs.append("filled tests.existing_goldens_unchanged with true")
    if plan.get("verdict") == "reject" and "approach" not in plan:
        plan["approach"] = "none"
        repairs.append("filled approach with none for a reject")
    for key in list(plan):
        if key not in schema("plan.schema.json")["properties"]:
            plan.pop(key)
            repairs.append(f"dropped unknown field {key}")
    return plan, repairs


def extract_plan(text):
    """Return (plan, repairs, error). The plan is the last fenced JSON block with a `verdict` field."""
    candidates = []
    for block in _FENCE.findall(text or ""):
        try:
            parsed = json.loads(block)
        except ValueError:
            continue
        if isinstance(parsed, dict) and "verdict" in parsed:
            candidates.append(parsed)
    if not candidates:
        return None, [], "no fenced JSON block with a verdict field"
    plan, repairs = repair_plan(candidates[-1])
    errors = validate(plan, schema("plan.schema.json"), schema("plan.schema.json"))
    if errors:
        return None, repairs, "; ".join(errors[:5])
    return normalize_plan(plan), repairs, None
