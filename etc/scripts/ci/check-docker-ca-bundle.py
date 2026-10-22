#!/usr/bin/env python3
"""Verify that network-fetching Dockerfile RUN steps mount the optional ca_bundle
secret and that Justfiles and docker-bake.hcl gate it conditionally on DOCKER_CA_BUNDLE
with no default path.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]

MOUNT_RE = re.compile(r"--mount=(\S+)")
APT_RE = re.compile(r"(?<![/\w-])(?:apt-get|apt)(?=\s|$)")
NETWORK_RUN_RE = re.compile(
    r"(?:(?<![/\w-])(?:apt-get|apt)(?=\s|$)"
    r"|\b(?:apk\s+add|git\s+(?:clone|fetch|submodule)"
    r"|go\s+(?:install|build|mod\s+download)|cargo\s+(?:\+\S+\s+)?(?:install|chef\s+cook|build)"
    r"|curl|wget|rustup-init|foundryup)\b)"
)
CONDITIONAL_SECRET_FILE_GUARD_RE = re.compile(
    r"(?:\[\s*-[fsr]\s+['\"]?/run/secrets/ca_bundle['\"]?\s*\]"
    r"|(?<!\!\s)\btest\s+-[fsr]\s+['\"]?/run/secrets/ca_bundle['\"]?)"
)
INVERTED_SECRET_FILE_GUARD_RE = re.compile(
    r"(?:\[\s*!\s*-[fsr]\s+['\"]?/run/secrets/ca_bundle['\"]?\s*\]"
    r"|\btest\s+!\s+-[fsr]\s+['\"]?/run/secrets/ca_bundle['\"]?)"
)
BAKE_CONDITIONAL_SECRET_RE = re.compile(
    r'secret\s*=\s*DOCKER_CA_BUNDLE\s*!=\s*""\s*\?\s*\[\s*"[^"]*id=ca_bundle,src=\$\{DOCKER_CA_BUNDLE\}[^"]*"\s*\]\s*:\s*\[\s*\]'
)


def parse_logical_instructions(dockerfile_text: str) -> list[tuple[int, str]]:
    instructions: list[tuple[int, str]] = []
    cur_lines: list[str] = []
    start_line = 1
    for idx, raw in enumerate(dockerfile_text.splitlines(), start=1):
        stripped = raw.strip()
        if not cur_lines:
            if not stripped or stripped.startswith("#"):
                continue
            start_line = idx
        if stripped.startswith("#"):
            continue
        if raw.rstrip().endswith("\\"):
            cur_lines.append(raw.rstrip()[:-1] + " ")
        else:
            cur_lines.append(raw)
            instructions.append((start_line, "".join(cur_lines).strip()))
            cur_lines = []
    if cur_lines:
        instructions.append((start_line, "".join(cur_lines).strip()))
    return instructions


def parse_mount(spec: str) -> dict[str, str]:
    opts: dict[str, str] = {}
    for part in spec.strip("\"'").split(","):
        key, sep, value = part.partition("=")
        if key.strip():
            opts[key.strip().lower()] = value.strip() if sep else "true"
    return opts


def check_dockerfiles_in_map(dockerfile_texts: dict[str, str], errors: list[str]) -> set[str]:
    dockerfiles_with_mount: set[str] = set()
    if not dockerfile_texts:
        errors.append("No etc/docker/Dockerfile.* files found")
        return dockerfiles_with_mount

    for rel, text in sorted(dockerfile_texts.items()):
        if re.search(r"^\s*(?:COPY|ADD)\b.*ca[_-]bundle", text, re.MULTILINE):
            errors.append(f"{rel}: must not COPY/ADD custom CA into image layers")
        for line_no, instr in parse_logical_instructions(text):
            if not instr.upper().startswith("RUN "):
                continue
            mounts = [parse_mount(m) for m in MOUNT_RE.findall(instr)]
            ca_mounts = [m for m in mounts if m.get("type") == "secret" and m.get("id") == "ca_bundle"]
            if ca_mounts:
                dockerfiles_with_mount.add(rel)

            for m in ca_mounts:
                if m.get("required", "false").lower() != "false":
                    errors.append(f"{rel}:{line_no}: ca_bundle mount must set required=false")
                if m.get("target") != "/run/secrets/ca_bundle":
                    errors.append(f"{rel}:{line_no}: ca_bundle mount must set target=/run/secrets/ca_bundle")
                if INVERTED_SECRET_FILE_GUARD_RE.search(instr) or not CONDITIONAL_SECRET_FILE_GUARD_RE.search(instr):
                    errors.append(
                        f"{rel}:{line_no}: RUN with ca_bundle mount must guard usage with a positive file test (e.g. `[ -f /run/secrets/ca_bundle ]`)"
                    )
                if APT_RE.search(instr):
                    if m.get("mode") not in ("0444", "444", "0644", "644"):
                        errors.append(f"{rel}:{line_no}: apt RUN ca_bundle mount must set mode=0444")
                    if "Acquire::https::CaInfo=/run/secrets/ca_bundle" not in instr:
                        errors.append(f"{rel}:{line_no}: apt RUN must pass Acquire::https::CaInfo=/run/secrets/ca_bundle")

            if NETWORK_RUN_RE.search(instr) and not ca_mounts:
                errors.append(f"{rel}:{line_no}: network RUN missing optional ca_bundle secret mount: {instr[:120]}")
    return dockerfiles_with_mount


def check_dockerfiles(errors: list[str]) -> set[str]:
    dockerfile_paths = sorted((REPO_ROOT / "etc" / "docker").glob("Dockerfile.*"))
    return check_dockerfiles_in_map(
        {p.relative_to(REPO_ROOT).as_posix(): p.read_text(encoding="utf-8") for p in dockerfile_paths},
        errors,
    )


def eval_just_var(justfile: str, var: str, env: dict[str, str]) -> str:
    return subprocess.run(
        ["just", "--justfile", justfile, "--evaluate", var],
        cwd=REPO_ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=True,
    ).stdout


def check_justfiles(errors: list[str]) -> bool:
    ca_just = REPO_ROOT / "etc/just/ca-bundle.just"
    if not ca_just.exists():
        errors.append("etc/just/ca-bundle.just is missing")
        return False
    ca_just_text = ca_just.read_text(encoding="utf-8")
    if re.search(r"HOME|\.local|\.pem|\.crt", ca_just_text):
        errors.append("etc/just/ca-bundle.just must not hardcode a default CA bundle path")

    justfiles = ("etc/docker/Justfile", "etc/just/prover.just", "crates/proof/tee/Justfile")
    for rel in justfiles:
        text = (REPO_ROOT / rel).read_text(encoding="utf-8")
        if "ca-bundle.just" not in text:
            errors.append(f"{rel}: must import shared etc/just/ca-bundle.just")

    if shutil.which("just") is None:
        print("SKIP: 'just' not found on PATH; skipped Justfile variable evaluation checks.")
        return False

    with tempfile.TemporaryDirectory(prefix="ca-bundle-ci-") as tmp_dir:
        tmp_home = Path(tmp_dir) / "home"
        tmp_home.mkdir(parents=True, exist_ok=True)
        for rel_candidate in (".local/etc/ca-bundle.pem", ".local/etc/certs.pem"):
            p = tmp_home / rel_candidate
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text("dummy", encoding="utf-8")

        ca_file = Path(tmp_dir) / "ca.pem"
        ca_file.write_text("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n", encoding="utf-8")

        base_env = {**os.environ, "HOME": str(tmp_home)}
        base_env.pop("DOCKER_CA_BUNDLE", None)

        for rel in justfiles:
            if eval_just_var(rel, "_ca_bundle_secret", base_env) != "":
                errors.append(f"{rel}: _ca_bundle_secret should be empty when DOCKER_CA_BUNDLE is unset")
            if eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": ""}) != "":
                errors.append(f"{rel}: _ca_bundle_secret should be empty when DOCKER_CA_BUNDLE=''")
            if eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": str(tmp_home)}) != "":
                errors.append(f"{rel}: _ca_bundle_secret should be empty when DOCKER_CA_BUNDLE is a directory")
            if eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": str(Path(tmp_dir) / "missing.pem")}) != "":
                errors.append(f"{rel}: _ca_bundle_secret should be empty when DOCKER_CA_BUNDLE does not exist")
            if not eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": str(ca_file)}).startswith(
                "--secret id=ca_bundle,src="
            ):
                errors.append(f"{rel}: _ca_bundle_secret not populated when DOCKER_CA_BUNDLE names a regular file")
        rel_eval = eval_just_var("crates/proof/tee/Justfile", "_ca_bundle", {**base_env, "DOCKER_CA_BUNDLE": "Justfile"})
        if rel_eval != str((REPO_ROOT / "crates/proof/tee/Justfile").resolve()):
            errors.append(f"crates/proof/tee/Justfile: relative DOCKER_CA_BUNDLE not canonicalized: {rel_eval!r}")
    return True


def iter_hcl_targets(bake_text: str) -> list[tuple[str, str]]:
    targets: list[tuple[str, str]] = []
    for match in re.finditer(r'target\s+"([^"]+)"\s*\{', bake_text):
        name, depth, idx = match.group(1), 1, match.end()
        while idx < len(bake_text) and depth > 0:
            depth += (bake_text[idx] == "{") - (bake_text[idx] == "}")
            idx += 1
        targets.append((name, bake_text[match.end() : idx - 1]))
    return targets


def check_bake_text(bake_text: str, dockerfiles_with_mount: set[str], errors: list[str]) -> None:
    if 'variable "DOCKER_CA_BUNDLE"' not in bake_text:
        errors.append('etc/docker/docker-bake.hcl: missing variable "DOCKER_CA_BUNDLE"')
    for target_name, body in iter_hcl_targets(bake_text):
        df_match = re.search(r'dockerfile\s*=\s*"([^"]+)"', body)
        if df_match and df_match.group(1) in dockerfiles_with_mount and not BAKE_CONDITIONAL_SECRET_RE.search(body):
            errors.append(
                f"etc/docker/docker-bake.hcl: target '{target_name}' uses {df_match.group(1)} but lacks conditional DOCKER_CA_BUNDLE secret"
            )


def main() -> int:
    errors: list[str] = []
    dockerfiles_with_mount = check_dockerfiles(errors)
    just_evaluated = check_justfiles(errors)
    check_bake_text((REPO_ROOT / "etc/docker/docker-bake.hcl").read_text(encoding="utf-8"), dockerfiles_with_mount, errors)

    if errors:
        print(f"FAILED ({len(errors)} errors):", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    status = "all" if just_evaluated else "Dockerfile and static (just evaluation skipped)"
    print(f"OK ({status}): Dockerfile ca_bundle mounts and conditional build plumbing verified.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
