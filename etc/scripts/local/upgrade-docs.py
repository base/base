#!/usr/bin/env python3
"""Check the upgrade guide against the feature registry and its production consumers.

`sync` refreshes generated feature tables; `check` validates their structure and
contents. Semantic agreement between prose and implementation requires review.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
REGISTRY = "crates/common/genesis/src/feature.rs"
UPGRADES = "crates/common/genesis/src/chain/upgrade.rs"
GUIDE = "docs/guides/UPGRADES.md"
# Only schedule machinery and compatibility adapters may ask whether a named fork
# is active. Tests/benchmarks are excluded separately, not grandfathered here.
FORK_GATE_ADAPTERS = {
    REGISTRY,
    "crates/common/genesis/src/rollup.rs",
    "crates/common/chains/src/upgrades.rs",
    "crates/common/evm/src/spec.rs",
    "crates/common/evm2/src/spec.rs",
    "crates/common/chains/src/upgrade.rs",  # Resolves the EVM spec identity.
    "crates/common/genesis/src/schedule.rs",  # Validates a block-index schedule.
}


def mask_rust(text: str) -> str:
    """Blank comments and literals, retaining character offsets and line breaks.

    This is a lexical scan, not a Rust parser: it does not expand macros or
    evaluate cfg expressions. A quote starts a character literal only when one
    character (or escape) is followed by its closing quote, leaving lifetimes
    and labels intact.
    """
    tokens = re.compile(
        r'//|/\*|\b(?:br|cr|r)(?P<hashes>\#*)"'
        r"|(?:b)?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|[^\r\n])|[^'\\\r\n])'"
        r'|(?:b|c)?"(?:\\[\s\S]|[^"\\])*"'
    )
    delimiters = re.compile(r"/\*|\*/")
    parts = []
    offset = 0
    while match := tokens.search(text, offset):
        end = match.end()
        if match[0] == "//":
            end = text.find("\n", end)
            if end == -1:
                end = len(text)
        elif match[0] == "/*":
            depth = 1
            while depth and (delimiter := delimiters.search(text, end)):
                depth += 1 if delimiter[0] == "/*" else -1
                end = delimiter.end()
            if depth:
                end = len(text)
        elif match["hashes"] is not None:
            closing = '"' + match["hashes"]
            closing_start = text.find(closing, end)
            end = len(text) if closing_start == -1 else closing_start + len(closing)
        parts.append(text[offset:match.start()])
        parts.append(re.sub(r"[^\r\n]", " ", text[match.start():end]))
        offset = end
    parts.append(text[offset:])
    return "".join(parts)


def production(text: str) -> str:
    """Mask non-code and omit the colocated test module (required to be last)."""
    text = mask_rust(text)
    test_start = None
    offset = 0
    # Scan complete attribute lines rather than backtracking over an arbitrary
    # number of attributes. Attribute-like strings in source must not stall CI.
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        if (stripped.startswith("#[cfg(") and stripped.endswith(")]")
                and re.search(r"\btest\b", stripped)):
            if test_start is None:
                test_start = offset + line.index("#")
        elif test_start is not None:
            if re.match(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+tests\b", stripped):
                return text[:test_start]
            if stripped and not (stripped.startswith("#[") and stripped.endswith("]")):
                test_start = None
        offset += len(line)
    return text


def sources(root: Path) -> dict[str, str]:
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.rs"],
        cwd=root,
    ).decode().split("\0")
    return {
        path: production((root / path).read_text())
        for path in sorted(set(paths))
        if path and (root / path).is_file()
        and not set(Path(path).parts).intersection({"tests", "benches", "test_utils", "target"})
        and not path.endswith(("/tests.rs", "/test_utils.rs"))
    }


def registry(text: str) -> list[tuple[str, str, str]]:
    body = re.search(r"^features! \{\n(.*?)^\}", text, re.MULTILINE | re.DOTALL)
    if body is None:
        raise ValueError("feature registry is missing")
    result = []
    upgrade = None
    upgrades = set()
    docs = []
    for line in body[1].splitlines():
        line = line.strip()
        if not line:
            continue
        group = re.fullmatch(r"(\w+)\s*\{", line)
        feature = re.fullmatch(r"(\w+),", line)
        if upgrade is None and group and group[1] not in upgrades:
            upgrade = group[1]
            upgrades.add(upgrade)
        elif upgrade is not None and line.startswith("///"):
            docs.append(line.removeprefix("///").strip())
        elif upgrade is not None and feature and docs:
            result.append((feature[1], upgrade, " ".join(docs)))
            docs = []
        elif (upgrade is not None and line == "}," and not docs
              and result and result[-1][1] == upgrade):
            upgrade = None
        else:
            raise ValueError(f"malformed feature registry entry: {line}")
    if upgrade is not None or not result or len({entry[0] for entry in result}) != len(result):
        raise ValueError("feature registry is empty, malformed, or contains duplicate features")
    return result


def upgrade_names(text: str) -> list[str]:
    body = text.split("    BaseUpgrade {", 1)[1].split("\n    }", 1)[0]
    return re.findall(r"^        (\w+),$", body, re.MULTILINE)


def fork_gate_errors(code: dict[str, str]) -> list[str]:
    """Catch common ways to bypass the registry; this is not a Rust semantic lint."""
    names = upgrade_names(code[UPGRADES])
    snake_names = [re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower() for name in names]
    pattern = re.compile(
        rf"\bis_(?:first_)?(?:base_)?(?:{'|'.join(snake_names)})_"
        r"(?:active(?:_at_(?:timestamp|block))?|block)\s*\("
        r"|\bis_enabled_in\s*\(\s*BaseUpgrade::"
        rf"|(?<![=\-])(?:>=|<=|>|<)\s*\(?BaseUpgrade::(?:{'|'.join(names)})\b"
    )
    errors = []
    for path, text in code.items():
        if path in FORK_GATE_ADAPTERS:
            continue
        for match in pattern.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            errors.append(f"{path}:{line}: Base behavior must use a feature gate: {match[0]}")
    return errors


def inventory(root: Path) -> dict[str, str]:
    code = sources(root)
    # Registry descriptions come from doc comments, unlike production uses.
    entries = registry((root / REGISTRY).read_text())
    upgrades = upgrade_names(code[UPGRADES])
    if not upgrades:
        raise ValueError("no BaseUpgrade variants found")
    unknown = {upgrade for _, upgrade, _ in entries} - set(upgrades)
    if unknown:
        raise ValueError(f"features refer to unknown upgrades: {sorted(unknown)}")
    known = {feature for feature, _, _ in entries}
    used = {
        name for path, text in code.items() if path != REGISTRY
        for name in re.findall(r"BaseFeature::(\w+)", text)
    } - {"ALL", "activation_states"}
    if used - known:
        raise ValueError(f"unregistered feature uses: {sorted(used - known)}")
    consumers = {
        feature: [
            path for path, text in code.items()
            if path != REGISTRY and re.search(rf"\bBaseFeature::{feature}\b", text)
        ]
        for feature in known
    }
    result = {}
    for upgrade in upgrades:
        rows = []
        for feature, activation, description in entries:
            if activation != upgrade:
                continue
            paths = consumers[feature]
            if not paths:
                raise ValueError(f"{feature} has no production consumers")
            links = ", ".join(f"[`{path}`](../../{path})" for path in paths)
            rows.append(f"| `{feature}` | {description} | {links} |")
        result[upgrade] = "\n".join([
            "| Feature | Behavior | Production code |",
            "| --- | --- | --- |",
            *rows,
        ]) if rows else "No separately gated Base features; see the baseline description above."
    return result


def update_guide(text: str, data: dict[str, str]) -> str:
    documented = re.findall(r"<!-- FEATURES:(\w+):START -->", text)
    if len(documented) != len(set(documented)) or set(documented) != data.keys():
        raise ValueError("guide must have exactly one FEATURES block per BaseUpgrade variant")
    for upgrade, table in data.items():
        pattern = rf"(<!-- FEATURES:{upgrade}:START -->\n).*?(<!-- FEATURES:{upgrade}:END -->)"
        text, count = re.subn(pattern, lambda m: m[1] + table + "\n" + m[2], text, flags=re.DOTALL)
        if count != 1:
            raise ValueError(f"missing or malformed feature block for {upgrade}")
    return text


def check(root: Path) -> list[str]:
    data = inventory(root)
    text = (root / GUIDE).read_text()
    errors = fork_gate_errors(sources(root))
    if update_guide(text, data) != text:
        errors.append("feature inventory is stale; run upgrade-docs.py sync")
    for link in re.findall(r"\]\((\.\.?/[^)#]+)(?:#[^)]*)?\)", text):
        if not (root / GUIDE).parent.joinpath(link).is_file():
            errors.append(f"broken code/documentation link: {link}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "sync"))
    args = parser.parse_args()
    try:
        if args.command == "check":
            errors = check(ROOT)
            if errors:
                print("\n".join(errors), file=sys.stderr)
                return 1
            print("Upgrade docs match the feature registry and detected production consumers.")
        else:
            path = ROOT / GUIDE
            path.write_text(update_guide(path.read_text(), inventory(ROOT)))
            print("Updated upgrade inventory; review semantic documentation changes separately.")
    except (ValueError, KeyError, IndexError) as error:
        print(f"upgrade-docs: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
