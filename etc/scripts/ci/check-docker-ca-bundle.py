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
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]

MOUNT_RE = re.compile(r"--mount=(\S+)")
APT_RE = re.compile(r"(?<![/\w-])(?:apt-get|apt)(?=\s|$)")
NETWORK_RUN_RE = re.compile(
    r"(?:(?<![/\w-])(?:apt-get|apt)(?=\s|$)"
    r"|\b(?:apk\s+add|git\s+(?:clone|fetch|submodule)"
    r"|go\s+(?:install|build|mod\s+download)"
    r"|cargo\s+(?:\+\S+\s+)?(?:chef|install|build|fetch)"
    r"|pip3?\s+install|(?:npm|yarn|pnpm)\s+(?:install|ci|add)"
    r"|curl|wget|rustup(?:-init)?|foundryup)\b)"
)
RUN_PREFIX_RE = re.compile(r"^RUN\s+(?:--mount=\S+\s+)*", re.IGNORECASE)
RUST_TOOLCHAIN_CMD_RE = re.compile(
    r"(?:^|&&|\|\||[;|(`]|\b(?:then|else|do)\b)\s*"
    r"(?:[A-Za-z_][A-Za-z0-9_]*=(?:\"[^\"]*\"|'[^']*'|[^\s\"']+)?\s+)*"
    r"(?:/[\w./-]+/)?(?:cargo|rustc|rustup)(?=\s|[;&|)]|$)"
)
FROM_RE = re.compile(r"^FROM\s+(?:--\S+\s+)*(\S+)(?:\s+AS\s+(\S+))?", re.IGNORECASE)
COPY_RUST_TOOLCHAIN_RE = re.compile(r"^(?:COPY|ADD)\b.*rust-toolchain(?:\.toml)?\b", re.IGNORECASE)
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
CANONICAL_CA_SHELL = (
    'SHELL ["/bin/sh", "-c", '
    '"if [ -f /run/secrets/ca_bundle ]; then '
    "export SSL_CERT_FILE=/run/secrets/ca_bundle "
    "CURL_CA_BUNDLE=/run/secrets/ca_bundle "
    "CARGO_HTTP_CAINFO=/run/secrets/ca_bundle "
    "GIT_SSL_CAINFO=/run/secrets/ca_bundle "
    "NODE_EXTRA_CA_CERTS=/run/secrets/ca_bundle; "
    'fi; exec /bin/sh -c \\"$1\\"", "--"]'
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


def collect_output_stages() -> dict[str, set[str]]:
    output_stages: dict[str, set[str]] = {}
    bake_path = REPO_ROOT / "etc/docker/docker-bake.hcl"
    if bake_path.exists():
        target_bodies = dict(iter_hcl_targets(bake_path.read_text(encoding="utf-8")))

        def resolve_dockerfile(name: str, seen: set[str] | None = None) -> str | None:
            visited = seen or set()
            if name in visited or name not in target_bodies:
                return None
            visited.add(name)
            body = target_bodies[name]
            df_match = re.search(r'dockerfile\s*=\s*"([^"]+)"', body)
            if df_match:
                return df_match.group(1)
            inh_match = re.search(r"inherits\s*=\s*\[([^\]]*)\]", body)
            if not inh_match:
                return None
            for parent in re.findall(r'"([^"]+)"', inh_match.group(1)):
                resolved = resolve_dockerfile(parent, visited)
                if resolved:
                    return resolved
            return None

        for target_name, body in target_bodies.items():
            stage_match = re.search(r'target\s*=\s*"([^"]+)"', body)
            dockerfile = resolve_dockerfile(target_name)
            if stage_match and dockerfile:
                output_stages.setdefault(dockerfile, set()).add(stage_match.group(1))

    for jf_rel in ("Justfile", "etc/docker/Justfile", "etc/just/prover.just", "crates/proof/tee/Justfile"):
        jf_path = REPO_ROOT / jf_rel
        if not jf_path.exists():
            continue
        for line in jf_path.read_text(encoding="utf-8").splitlines():
            df_m = re.search(r"-f\s+(etc/docker/Dockerfile\.\S+)", line)
            tgt_m = re.search(r"--target(?:\s+|=)([A-Za-z0-9_.-]+)", line)
            if df_m and tgt_m:
                output_stages.setdefault(df_m.group(1), set()).add(tgt_m.group(1))

    return output_stages


def check_dockerfiles_in_map(
    dockerfile_texts: dict[str, str],
    errors: list[str],
    output_stages_by_dockerfile: dict[str, set[str]] | None = None,
) -> set[str]:
    output_stages_map = output_stages_by_dockerfile if output_stages_by_dockerfile is not None else collect_output_stages()
    dockerfiles_with_mount: set[str] = set()
    if not dockerfile_texts:
        errors.append("No etc/docker/Dockerfile.* files found")
        return dockerfiles_with_mount

    for rel, text in sorted(dockerfile_texts.items()):
        if re.search(r"^\s*(?:COPY|ADD)\b.*ca[_-]bundle", text, re.MULTILINE):
            errors.append(f"{rel}: must not COPY/ADD custom CA into image layers")
        if re.search(r"^\s*ENV\b.*/run/secrets/ca_bundle", text, re.MULTILINE):
            errors.append(f"{rel}: must not set ENV referencing /run/secrets/ca_bundle")
        stages_with_rust_toolchain: set[str] = set()
        stages_with_ca_shell: set[str] = set()
        current_stage: str | None = None
        current_has_rust_toolchain = False
        current_has_ca_shell = False

        for line_no, instr in parse_logical_instructions(text):
            from_match = FROM_RE.match(instr)
            if from_match:
                base_ref = from_match.group(1)
                current_stage = from_match.group(2)
                current_has_rust_toolchain = base_ref in stages_with_rust_toolchain
                current_has_ca_shell = base_ref in stages_with_ca_shell
                if current_stage and current_has_rust_toolchain:
                    stages_with_rust_toolchain.add(current_stage)
                if current_stage and current_has_ca_shell:
                    stages_with_ca_shell.add(current_stage)
                continue
            if instr.upper().startswith("SHELL "):
                if instr == CANONICAL_CA_SHELL:
                    if current_stage in ("runtime-base", "describe") or current_stage is None:
                        errors.append(f"{rel}:{line_no}: runtime stage must not set a ca_bundle SHELL")
                    current_has_ca_shell = True
                    if current_stage:
                        stages_with_ca_shell.add(current_stage)
                elif "ca_bundle" in instr:
                    errors.append(f"{rel}:{line_no}: ca_bundle SHELL must match CANONICAL_CA_SHELL")
                continue
            if COPY_RUST_TOOLCHAIN_RE.match(instr):
                current_has_rust_toolchain = True
                if current_stage:
                    stages_with_rust_toolchain.add(current_stage)
                continue
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
                has_inline_guard = bool(CONDITIONAL_SECRET_FILE_GUARD_RE.search(instr))
                has_inverted_guard = bool(INVERTED_SECRET_FILE_GUARD_RE.search(instr))
                if APT_RE.search(instr):
                    if has_inverted_guard or not has_inline_guard:
                        errors.append(
                            f"{rel}:{line_no}: apt RUN with ca_bundle mount must guard usage with a positive file test"
                        )
                    if m.get("mode") not in ("0444", "444", "0644", "644"):
                        errors.append(f"{rel}:{line_no}: apt RUN ca_bundle mount must set mode=0444")
                    if "Acquire::https::CaInfo=/run/secrets/ca_bundle" not in instr:
                        errors.append(f"{rel}:{line_no}: apt RUN must pass Acquire::https::CaInfo=/run/secrets/ca_bundle")
                elif has_inverted_guard or not (has_inline_guard or current_has_ca_shell):
                    errors.append(
                        f"{rel}:{line_no}: RUN with ca_bundle mount must guard usage with a positive file test or stage SHELL"
                    )

            cmd_body = RUN_PREFIX_RE.sub("", instr, count=1)
            needs_ca = bool(NETWORK_RUN_RE.search(cmd_body)) or (
                current_has_rust_toolchain and bool(RUST_TOOLCHAIN_CMD_RE.search(cmd_body))
            )
            if needs_ca and not ca_mounts:
                errors.append(f"{rel}:{line_no}: network RUN missing optional ca_bundle secret mount: {instr[:120]}")
        if current_has_ca_shell:
            errors.append(f"{rel}: final stage must not set or inherit a ca_bundle SHELL")
        for out_stage in sorted(output_stages_map.get(rel, set())):
            if out_stage in stages_with_ca_shell:
                errors.append(f"{rel}: output stage '{out_stage}' must not set or inherit a ca_bundle SHELL")
    return dockerfiles_with_mount


def check_dockerfiles(errors: list[str]) -> set[str]:
    dockerfile_paths = sorted((REPO_ROOT / "etc" / "docker").glob("Dockerfile.*"))
    return check_dockerfiles_in_map(
        {p.relative_to(REPO_ROOT).as_posix(): p.read_text(encoding="utf-8") for p in dockerfile_paths},
        errors,
    )


def eval_just_var(
    justfile: str, var: str, env: dict[str, str], cwd: Path = REPO_ROOT
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["just", "--justfile", str(REPO_ROOT / justfile), "--evaluate", var],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=True,
    )


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

        ca_dir = Path(tmp_dir) / "dir with space"
        ca_dir.mkdir(parents=True, exist_ok=True)
        ca_file = ca_dir / "custom ca.pem"
        ca_file.write_text("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n", encoding="utf-8")
        expected_ca_file = str(ca_file.resolve())

        base_env = {**os.environ, "HOME": str(tmp_home)}
        base_env.pop("DOCKER_CA_BUNDLE", None)

        sub_dir = Path(tmp_dir) / "subdir"
        sub_dir.mkdir(parents=True, exist_ok=True)
        sub_ca = sub_dir / "rel-ca.pem"
        sub_ca.write_text("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n", encoding="utf-8")
        expected_sub_ca = str(sub_ca.resolve())

        for rel in justfiles:
            res_unset = eval_just_var(rel, "_ca_bundle_secret", base_env)
            if res_unset.stdout != "" or res_unset.stderr != "":
                errors.append(f"{rel}: unset DOCKER_CA_BUNDLE must yield empty stdout/stderr, got {res_unset.stdout!r} / {res_unset.stderr!r}")
            res_allow_unset = eval_just_var(rel, "_ca_bundle_bake_allow", base_env)
            if res_allow_unset.stdout != "" or res_allow_unset.stderr != "":
                errors.append(f"{rel}: unset DOCKER_CA_BUNDLE must yield empty _ca_bundle_bake_allow, got {res_allow_unset.stdout!r}")

            res_empty = eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": ""})
            if res_empty.stdout != "" or res_empty.stderr != "":
                errors.append(f"{rel}: empty DOCKER_CA_BUNDLE='' must yield empty stdout/stderr, got {res_empty.stdout!r} / {res_empty.stderr!r}")

            res_dir = eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": str(tmp_home)})
            if res_dir.stdout != "" or "warning:" not in res_dir.stderr.lower():
                errors.append(f"{rel}: directory DOCKER_CA_BUNDLE must yield empty stdout and warning on stderr, got {res_dir.stdout!r} / {res_dir.stderr!r}")

            res_missing = eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": str(Path(tmp_dir) / "missing.pem")})
            if res_missing.stdout != "" or "warning:" not in res_missing.stderr.lower():
                errors.append(f"{rel}: missing DOCKER_CA_BUNDLE must yield empty stdout and warning on stderr, got {res_missing.stdout!r} / {res_missing.stderr!r}")

            res_abs = eval_just_var(rel, "_ca_bundle_secret", {**base_env, "DOCKER_CA_BUNDLE": str(ca_file)})
            if not res_abs.stdout.startswith("--secret id=ca_bundle,src=") or res_abs.stderr != "":
                errors.append(f"{rel}: regular-file DOCKER_CA_BUNDLE failed: {res_abs.stdout!r} / {res_abs.stderr!r}")
            res_allow = eval_just_var(rel, "_ca_bundle_bake_allow", {**base_env, "DOCKER_CA_BUNDLE": str(ca_file)})
            if res_allow.stdout != f"--allow=fs.read='{expected_ca_file}'" or res_allow.stderr != "":
                errors.append(f"{rel}: regular-file DOCKER_CA_BUNDLE _ca_bundle_bake_allow failed: {res_allow.stdout!r} / {res_allow.stderr!r}")

            res_rel = eval_just_var(rel, "_ca_bundle", {**base_env, "DOCKER_CA_BUNDLE": "rel-ca.pem"}, cwd=sub_dir)
            if res_rel.stdout != expected_sub_ca or res_rel.stderr != "":
                errors.append(
                    f"{rel}: relative DOCKER_CA_BUNDLE from subdirectory {sub_dir} resolved to {res_rel.stdout!r} (expected {expected_sub_ca!r}, stderr={res_rel.stderr!r})"
                )
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
    target_bodies = dict(iter_hcl_targets(bake_text))

    def target_has_secret(name: str, seen: set[str] | None = None) -> bool:
        visited = seen or set()
        if name in visited or name not in target_bodies:
            return False
        visited.add(name)
        body = target_bodies[name]
        if BAKE_CONDITIONAL_SECRET_RE.search(body):
            return True
        inh_match = re.search(r"inherits\s*=\s*\[([^\]]*)\]", body)
        if not inh_match:
            return False
        parents = re.findall(r'"([^"]+)"', inh_match.group(1))
        return any(target_has_secret(parent, visited) for parent in parents)

    for target_name, body in target_bodies.items():
        df_match = re.search(r'dockerfile\s*=\s*"([^"]+)"', body)
        if df_match and df_match.group(1) in dockerfiles_with_mount and not target_has_secret(target_name):
            errors.append(
                f"etc/docker/docker-bake.hcl: target '{target_name}' uses {df_match.group(1)} but lacks conditional DOCKER_CA_BUNDLE secret"
            )


def check_probes(errors: list[str]) -> None:
    probe_df = (
        "FROM rust:1.96-trixie AS base\n"
        "COPY rust-toolchain.toml ./\n"
        "RUN --mount=type=cache,target=/usr/local/cargo/registry mkdir -p /app/out\n"
        "RUN mkdir -p /app/cargo-cache && chmod 755 /usr/local/cargo/env\n"
    )
    probe_errs: list[str] = []
    check_dockerfiles_in_map({"etc/docker/Dockerfile.probe": probe_df}, probe_errs)
    if probe_errs:
        errors.append(f"probe: false positive on non-network cargo path RUNs: {probe_errs}")
    for env_probe in ("RUN X=$(cmd) rustc -vV\n", "RUN A=b;c rustc -vV\n"):
        env_probe_df = "FROM rust:1.96-trixie AS base\nCOPY rust-toolchain.toml ./\n" + env_probe
        env_probe_errs: list[str] = []
        check_dockerfiles_in_map({"etc/docker/Dockerfile.env-probe": env_probe_df}, env_probe_errs)
        if not env_probe_errs:
            errors.append(f"probe: unmounted `{env_probe.strip()}` in rust-toolchain stage was not flagged")

    rust_df = (REPO_ROOT / "etc/docker/Dockerfile.rust-services").read_text(encoding="utf-8")
    for label, pattern, repl in (
        (
            "shell",
            r'SHELL \["/bin/sh", "-c", "if \[ -f /run/secrets/ca_bundle \];[^\n]+\n',
            "",
        ),
        (
            "planner",
            r"FROM workspace-source AS planner\nRUN --mount=[^\n]+\n\s+cargo chef prepare --recipe-path recipe\.json",
            "FROM workspace-source AS planner\nRUN cargo chef prepare --recipe-path recipe.json",
        ),
        (
            "cook",
            r"--mount=type=secret,id=ca_bundle,target=/run/secrets/ca_bundle,required=false \\\n\s+(cargo chef cook)",
            r"\1",
        ),
        (
            "install",
            r"--mount=type=secret,id=ca_bundle,target=/run/secrets/ca_bundle,required=false \\\n\s+(cargo install cargo-chef)",
            r"\1",
        ),
    ):
        mutated, count = re.subn(pattern, repl, rust_df, count=1)
        if count != 1:
            errors.append(f"probe {label}: mutation pattern no longer matches Dockerfile.rust-services; update the probe")
            continue
        mut_errs: list[str] = []
        check_dockerfiles_in_map({"etc/docker/Dockerfile.rust-services": mutated}, mut_errs)
        if not mut_errs:
            errors.append(f"probe: stripping ca_bundle mount from {label} RUN in Dockerfile.rust-services did not fail")
    incomplete_shell_df = (
        "FROM public.ecr.aws/docker/library/golang:1.23-alpine3.20 AS builder\n"
        'SHELL ["/bin/sh", "-c", "if [ -f /run/secrets/ca_bundle ]; then export SSL_CERT_FILE=/run/secrets/ca_bundle GIT_SSL_CAINFO=/run/secrets/ca_bundle; fi; exec /bin/sh -c \\"$1\\"", "--"]\n'
        "RUN --mount=type=secret,id=ca_bundle,target=/run/secrets/ca_bundle,required=false \\\n"
        "    cargo install cargo-chef\n"
        "FROM public.ecr.aws/docker/library/alpine:3.20\n"
        "COPY --from=builder /bin/app /bin/app\n"
    )
    inc_errs: list[str] = []
    check_dockerfiles_in_map({"etc/docker/Dockerfile.proxyd": incomplete_shell_df}, inc_errs)
    if not inc_errs:
        errors.append("probe: non-canonical CA SHELL missing CARGO_HTTP_CAINFO did not fail")

    for out_stage in ("base", "execution"):
        mut_out = rust_df.replace(
            f"FROM runtime-base AS {out_stage}\n",
            f"FROM runtime-base AS {out_stage}\n{CANONICAL_CA_SHELL}\n",
            1,
        )
        if mut_out == rust_df:
            errors.append(f"probe {out_stage}: mutation pattern no longer matches Dockerfile.rust-services; update the probe")
            continue
        out_errs: list[str] = []
        check_dockerfiles_in_map({"etc/docker/Dockerfile.rust-services": mut_out}, out_errs)
        if not out_errs:
            errors.append(f"probe: adding CA SHELL to output stage '{out_stage}' in Dockerfile.rust-services did not fail")

    # Linear execution is ~1.2 ms at 5,000 repetitions, whereas exponential backtracking
    # exceeds 2.7 s at just 24 repetitions; 2.0 s avoids false positives on loaded runners.
    redos_input = "(A=" + '"" A=' * 5000 + "!"
    t0 = time.monotonic()
    RUST_TOOLCHAIN_CMD_RE.search(redos_input)
    elapsed = time.monotonic() - t0
    if elapsed > 2.0:
        errors.append(f"probe: RUST_TOOLCHAIN_CMD_RE took {elapsed:.3f}s on 5,000 repetitions (> 2.000s)")



def main() -> int:
    errors: list[str] = []
    dockerfiles_with_mount = check_dockerfiles(errors)
    just_evaluated = check_justfiles(errors)
    check_bake_text((REPO_ROOT / "etc/docker/docker-bake.hcl").read_text(encoding="utf-8"), dockerfiles_with_mount, errors)
    check_probes(errors)

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
