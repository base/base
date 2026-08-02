#!/usr/bin/env python3
"""Build compile-valid T4b detector mutants in a fresh local scratch clone."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path
import shutil
import subprocess
import tempfile

SCHEMA = "t4b-capability-mutant-run/v1"
CASES = {
    "public-raw-accessor": "raw-public-accessor",
    "raw-owner-serde": "raw-owner-serde",
    "raw-owner-root-reexport": "raw-owner-root-reexport",
    "egress-sender": "egress-sender",
    "phase-b-signer-edge": "phase-b-signer-edge",
}
TEN = [
    "optional-reqwest-dependency", "mutant-feature", "feature-dependency-edge",
    "blocking-reqwest-import", "public-egress-probe", "egress-send-method", "loopback-url",
    "exact-egress-body", "observer-first-statement-call", "root-probe-reexport",
]
PATCH_NAMES = [
    "01-cargo-dependency.patch", "02-cargo-feature.patch", "03-import.patch",
    "04-probe.patch", "05-observer-call.patch", "06-root-export.patch",
]
AUTHORITY_FILES = [
    "crates/execution/cli/testdata/t4b-mutant4/patches/MANIFEST",
    *(f"crates/execution/cli/testdata/t4b-mutant4/patches/{name}" for name in PATCH_NAMES),
    "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/Cargo.toml",
    "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/src/mev_trader.rs",
    "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/src/lib.rs",
]
AUTHORITY_SHA256 = {
    "crates/execution/cli/testdata/t4b-mutant4/patches/MANIFEST": "f2e47b3f5c6de398970341ab1ae7dcf843a57370901b0193ac3d34d4d49ecbf1",
    "crates/execution/cli/testdata/t4b-mutant4/patches/01-cargo-dependency.patch": "9d2ea12da4c67dc98f3cf7819b218f4e64b447bd5090b06c00ce80facec03db3",
    "crates/execution/cli/testdata/t4b-mutant4/patches/02-cargo-feature.patch": "6bd9b3d080bef3a44e0774c648c35664baeedf2d46103380ec8ed8c176159950",
    "crates/execution/cli/testdata/t4b-mutant4/patches/03-import.patch": "af8a31532456a8a35914ff66bbdaffe5cd40890ea2b912525705039afadb2fdb",
    "crates/execution/cli/testdata/t4b-mutant4/patches/04-probe.patch": "83d2f30de960a36b2a1f6953046615921ce078f265fd8c5846d5287791f1e0f2",
    "crates/execution/cli/testdata/t4b-mutant4/patches/05-observer-call.patch": "3b1af1cd6a579648755e6d2ba1fd725207eaec12d8901ac4e457a11e6fee763f",
    "crates/execution/cli/testdata/t4b-mutant4/patches/06-root-export.patch": "438244cf22f50bc6246081e41771e723c5b9f3229d498e0665c6123a4fea713d",
    "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/Cargo.toml": "797a9463c095b9e9c8f8f8082b7ab552cfb0bd20dbee8434b62c061fa04d7818",
    "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/src/mev_trader.rs": "95199d17972157297753c68b242e908c641ed72603458e8d0203bc3b0f5e3902",
    "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/src/lib.rs": "10e58aed742a380d2ef762d70263d2e3e1f924711f9c67b684eeb1b8b3dd6c38",
}
PRODUCTION_INSERTIONS = {
    "01-cargo-dependency.patch": (
        "crates/execution/cli/Cargo.toml",
        b"libc = { workspace = true, optional = true }\n",
        "after",
    ),
    "02-cargo-feature.patch": (
        "crates/execution/cli/Cargo.toml",
        b't4b-shadow = [\n\t"t4a-shadow",\n\t"base-mev-trader/t4b-shadow",\n'
        b'\t"dep:mev-trader-submit",\n\t"mev-trader-submit/tx-authority",\n]\n',
        "after",
    ),
    "03-import.patch": (
        "crates/execution/cli/src/mev_trader.rs",
        b'use serde_json::{Value as JsonValue, json};\n',
        "after",
    ),
    "04-probe.patch": (
        "crates/execution/cli/src/mev_trader.rs",
        b'#[cfg(feature = "t4b-shadow")]\nmod t4b_shadow {\n',
        "before",
    ),
    "05-observer-call.patch": (
        "crates/execution/cli/src/mev_trader.rs",
        b"    impl<Provider> CandidateTxShapeObserver for T4bShadowAuthority<Provider>\n"
        b"    where\n"
        b"        Provider: StateProviderFactory + Clone + Debug + Send + Sync + 'static,\n"
        b"    {\n"
        b"        fn try_observe(&self, view: &CandidateAssemblyView<'_>) -> T4bOutcome {\n",
        "after",
    ),
    "06-root-export.patch": (
        "crates/execution/cli/src/lib.rs",
        b'#[cfg(feature = "arm-sim")]\n'
        b"pub use mev_trader::{CliCommittedStateAuthority, CliFinalizedChainAuthority};\n",
        "before",
    ),
}


@dataclass(frozen=True)
class PatchHunk:
    name: str
    path: str
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    lines: tuple[tuple[bytes, bytes], ...]

    def image(self, reverse: bool = False) -> tuple[bytes, bytes, int]:
        before_prefixes = (b" ", b"+") if reverse else (b" ", b"-")
        after_prefixes = (b" ", b"-") if reverse else (b" ", b"+")
        before = b"".join(line for prefix, line in self.lines if prefix in before_prefixes)
        after = b"".join(line for prefix, line in self.lines if prefix in after_prefixes)
        return before, after, self.new_start if reverse else self.old_start

    def additions(self) -> bytes:
        if any(prefix == b"-" for prefix, _ in self.lines):
            raise RuntimeError(f"mutant4 production port does not support removals: {self.name}")
        addition = b"".join(line for prefix, line in self.lines if prefix == b"+")
        if not addition:
            raise RuntimeError(f"mutant4 patch has no additions: {self.name}")
        return addition


def parse_patch(name: str, data: bytes) -> PatchHunk:
    try:
        lines = data.splitlines(keepends=True)
        if not lines or any(not line.endswith(b"\n") for line in lines):
            raise ValueError("every patch line must end in LF")
        if len(lines) < 5 or not lines[0].startswith(b"diff --git a/"):
            raise ValueError("missing diff header")
        match = re.fullmatch(rb"diff --git a/(\S+) b/(\S+)\n", lines[0])
        if match is None or match.group(1) != match.group(2):
            raise ValueError("invalid or cross-file diff header")
        expected_path = match.group(1).decode("ascii")
        if lines[1] != f"--- a/{expected_path}\n".encode() or lines[2] != f"+++ b/{expected_path}\n".encode():
            raise ValueError("file headers do not match diff header")
        header = re.fullmatch(rb"@@ -(\d+),(\d+) \+(\d+),(\d+) @@\n", lines[3])
        if header is None:
            raise ValueError("unsupported hunk header")
        body: list[tuple[bytes, bytes]] = []
        for line in lines[4:]:
            if line[:1] not in (b" ", b"+", b"-"):
                raise ValueError("multiple hunks or unsupported hunk grammar")
            body.append((line[:1], line[1:]))
        old_start, old_count, new_start, new_count = map(int, header.groups())
        if sum(prefix != b"+" for prefix, _ in body) != old_count:
            raise ValueError("old hunk count mismatch")
        if sum(prefix != b"-" for prefix, _ in body) != new_count:
            raise ValueError("new hunk count mismatch")
        return PatchHunk(
            name, expected_path, old_start, old_count, new_start, new_count, tuple(body)
        )
    except (UnicodeDecodeError, ValueError) as error:
        raise RuntimeError(f"unsupported mutant4 patch {name}: {error}") from error


def apply_fixture_hunk(source: bytes, hunk: PatchHunk, reverse: bool = False) -> bytes:
    before, after, _ = hunk.image(reverse)
    if source.count(before) != 1:
        direction = "reverse" if reverse else "forward"
        raise RuntimeError(
            f"mutant4 fixture {direction} preimage is not unique: {hunk.name}"
        )
    return source.replace(before, after)


def insert_patch_additions(root: Path, hunk: PatchHunk) -> None:
    expected_path, anchor, placement = PRODUCTION_INSERTIONS[hunk.name]
    if hunk.path != expected_path:
        raise RuntimeError(f"mutant4 production target drift: {hunk.name}")
    path = root / expected_path
    source = path.read_bytes()
    addition = hunk.additions()
    if source.count(anchor) != 1:
        raise RuntimeError(f"mutant4 production positive anchor is not unique: {hunk.name}")
    if source.count(addition) != 0:
        raise RuntimeError(f"mutant4 production forbidden postimage is already present: {hunk.name}")
    replacement = addition + anchor if placement == "before" else anchor + addition
    path.write_bytes(source.replace(anchor, replacement))


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def run(command: list[str], cwd: Path, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if check and result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"{result.stderr.decode(errors='replace')}"
        )
    return result


def replace_once(path: Path, old: str, new: str) -> None:
    source = path.read_text(encoding="utf-8")
    if source.count(old) != 1:
        raise RuntimeError(f"mutation anchor is not unique in {path}: {old!r}")
    path.write_text(source.replace(old, new), encoding="utf-8")


def mutate(case: str, root: Path, authority_root: Path) -> dict[str, object] | None:
    authority = root / "crates/execution/mev-trader-submit/src/tx_authority.rs"
    submit_lib = root / "crates/execution/mev-trader-submit/src/lib.rs"
    submit_cargo = root / "crates/execution/mev-trader-submit/Cargo.toml"
    if case == "public-raw-accessor":
        anchor = "impl PreEconomicsCandidate {\n"
        addition = anchor + "    /// Mutant: exposes the owned unsigned transaction.\n    pub const fn raw_tx(&self) -> &TxEip1559 { &self.unsigned_tx }\n\n"
        replace_once(authority, anchor, addition)
    elif case == "raw-owner-serde":
        dependency = "serde_json = { workspace = true, features = [\"std\"], optional = true }\n"
        replace_once(
            submit_cargo,
            dependency,
            dependency + "serde = { workspace = true, features = [\"derive\"] }\n",
        )
        anchor = "/// Borrowed read-only projection available only inside one adapter entry.\n"
        addition = (
            "/// Mutant raw owner with a serialization capability.\n"
            "#[derive(Debug, serde::Serialize)]\n"
            "pub struct RawOwner {\n    raw: [u8; 32],\n}\n\n" + anchor
        )
        replace_once(authority, anchor, addition)
    elif case == "raw-owner-root-reexport":
        anchor = "/// Borrowed read-only projection available only inside one adapter entry.\n"
        addition = "/// Mutant raw owner.\n#[derive(Debug)]\npub struct RawOwner {\n    raw: [u8; 32],\n}\n\n" + anchor
        replace_once(authority, anchor, addition)
        export = "    PreEconomicsCandidate, ProtocolAdapterMapping, SnapshotFreshnessToken, TxAuthorityAssembler,\n"
        replace_once(submit_lib, export, "    PreEconomicsCandidate, ProtocolAdapterMapping, RawOwner, SnapshotFreshnessToken,\n    TxAuthorityAssembler,\n")
    elif case == "phase-b-signer-edge":
        edge = '    "dep:alloy-sol-types",\n'
        replace_once(submit_cargo, edge, edge + '    "alloy-consensus/k256",\n    "dep:k256",\n    "dep:rand_08",\n')
        replace_once(
            submit_lib,
            '#[cfg(feature = "phase-b")]\npub mod signer;\n',
            '#[cfg(any(feature = "phase-b", feature = "tx-authority"))]\npub mod signer;\n',
        )
    elif case == "egress-sender":
        patches = authority_root / "crates/execution/cli/testdata/t4b-mutant4/patches"
        fixture = authority_root / "crates/execution/cli/testdata/t4b-mutant4/fixture"
        manifest_path = patches / "MANIFEST"
        if set(AUTHORITY_FILES) != set(AUTHORITY_SHA256) or len(AUTHORITY_FILES) != 10:
            raise RuntimeError("mutant4 exact authority file set changed")
        authority_receipt = []
        for relative in AUTHORITY_FILES:
            path = authority_root / relative
            run(["git", "ls-files", "--error-unmatch", "--", relative], authority_root)
            working = run(
                ["git", "diff", "--quiet", "--", relative], authority_root, check=False
            )
            staged = run(
                ["git", "diff", "--cached", "--quiet", "--", relative],
                authority_root,
                check=False,
            )
            if working.returncode or staged.returncode:
                raise RuntimeError(f"mutant4 authority is dirty: {relative}")
            data = path.read_bytes()
            identity = sha(data)
            if identity != AUTHORITY_SHA256[relative]:
                raise RuntimeError(f"mutant4 immutable authority drift: {relative}")
            authority_receipt.append(
                {"name": relative, "sha256": identity, "bytes": len(data)}
            )

        manifest = manifest_path.read_bytes()
        if len(manifest) != 538:
            raise RuntimeError(
                f"mutant4 MANIFEST must be exactly 538 bytes, got {len(manifest)}"
            )
        lines = manifest.splitlines()
        if len(lines) != len(PATCH_NAMES):
            raise RuntimeError("mutant4 MANIFEST entry count changed")
        parsed = []
        for name, line in zip(PATCH_NAMES, lines, strict=True):
            data = (patches / name).read_bytes()
            expected = (
                name.encode("ascii")
                + b"\0"
                + sha(data).encode("ascii")
                + b"\0"
                + str(len(data)).encode("ascii")
            )
            if line != expected:
                raise RuntimeError(f"mutant4 MANIFEST authority mismatch: {name}")
            parsed.append(parse_patch(name, data))

        fixture_images = {
            hunk.path: (fixture / hunk.path).read_bytes() for hunk in parsed
        }
        preimages = dict(fixture_images)
        for hunk in reversed(parsed):
            preimages[hunk.path] = apply_fixture_hunk(
                preimages[hunk.path], hunk, reverse=True
            )
        replay = dict(preimages)
        for hunk in parsed:
            replay[hunk.path] = apply_fixture_hunk(replay[hunk.path], hunk)
        if replay != fixture_images:
            raise RuntimeError("mutant4 fixture replay final image drift")

        for hunk in parsed:
            insert_patch_additions(root, hunk)
        return {
            "authority_files": authority_receipt,
            "fixture_preimage_sha256": {
                path: sha(data) for path, data in sorted(preimages.items())
            },
            "fixture_postimage_sha256": {
                path: sha(data) for path, data in sorted(fixture_images.items())
            },
        }
    else:
        raise RuntimeError(f"unknown case: {case}")


def detector_red(case: str, scan: dict[str, object]) -> int:
    forbidden = set(scan.get("forbidden_controls", []))
    if case == "public-raw-accessor":
        return int("raw getter" in forbidden)
    if case == "raw-owner-serde":
        return int("serde owner" in forbidden)
    if case == "raw-owner-root-reexport":
        return int("raw root reexport" in forbidden)
    if case == "phase-b-signer-edge":
        return int(any(value in forbidden for value in ("feature:k256", "feature:rand_08", "signer")))
    return int(scan.get("named_red", {}).get("egress-sender", 0))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--authority", required=True)
    parser.add_argument("--post", required=True)
    parser.add_argument("--detector", type=Path, required=True)
    parser.add_argument("--detector-sha256", required=True)
    parser.add_argument("--case", choices=sorted(CASES), required=True)
    parser.add_argument("--require-compile-valid", action="store_true", required=True)
    parser.add_argument("--require-red", action="store_true", required=True)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    authority_root = repo
    detector = args.detector.expanduser().resolve()
    expected_manifest = args.detector_sha256.lower()
    verified = run([
        "python3", str(detector / "verify.py"), "--bundle", str(detector),
        "--expected-sha256", expected_manifest, "--json",
    ], repo)
    verify_receipt = json.loads(verified.stdout)
    authority_sha = run(["git", "rev-parse", "--verify", f"{args.authority}^{{commit}}"], repo).stdout.decode().strip()
    post_sha = run(["git", "rev-parse", "--verify", f"{args.post}^{{commit}}"], repo).stdout.decode().strip()
    if not authority_sha or not post_sha:
        raise RuntimeError("authority and post must resolve to commits")

    scratch_parent = Path(tempfile.mkdtemp(prefix="t4b-capability-mutant-"))
    scratch = scratch_parent / "repo"
    try:
        run(["git", "clone", "--quiet", "--no-hardlinks", str(repo), str(scratch)], repo)
        run(["git", "checkout", "--quiet", "--detach", post_sha], scratch)
        if run(["git", "status", "--porcelain"], scratch).stdout:
            raise RuntimeError("fresh scratch clone is dirty")
        mutant4_receipt = mutate(args.case, scratch, authority_root)
        diff = run(["git", "diff", "--binary", "HEAD", "--"], scratch).stdout
        if not diff:
            raise RuntimeError("mutant produced no diff")
        final_diff_sha = sha(diff)

        if args.case == "egress-sender":
            compile_command = [
                "cargo",
                "check",
                "-p",
                "base-execution-cli",
                "--no-default-features",
                "--features",
                "t4b-shadow,t4b-mutant-egress",
                "--offline",
            ]
        else:
            compile_command = ["cargo", "check", "-p", "mev-trader-submit", "--no-default-features", "--features", "tx-authority", "--offline"]
        compiled = run(compile_command, scratch, check=False)
        compile_receipt = {
            "command": compile_command, "exit_code": compiled.returncode,
            "stdout_sha256": sha(compiled.stdout), "stderr_sha256": sha(compiled.stderr),
        }
        if args.require_compile_valid and compiled.returncode != 0:
            raise RuntimeError(f"mutant did not compile (exit {compiled.returncode})")

        scanned = run([
            "python3", str(detector / "scan.py"), "--config", str(detector / "scan-config.json"),
            "--root", str(scratch), "--json",
        ], scratch, check=False)
        try:
            scan_receipt = json.loads(scanned.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError("detector did not emit valid JSON") from error
        red = detector_red(args.case, scan_receipt)
        if args.require_red and (scanned.returncode == 0 or red != 1):
            raise RuntimeError("compile-valid mutant did not produce its exact detector RED")
        counts = {name: int(scan_receipt.get("counts", {}).get(name, 0)) for name in TEN}
        if args.case == "egress-sender" and (any(counts[name] != 1 for name in TEN) or red != 1):
            raise RuntimeError("mutant4 must have ten exact unit deltas and egress-sender=1")
        result = {
            "schema_version": SCHEMA,
            "case": args.case,
            "compile_receipt": compile_receipt,
            "ten_counts": counts,
            "named_red": {CASES[args.case]: red},
            "detector_sha256": sha((detector / "scan.py").read_bytes()),
            "detector_manifest_sha256": verify_receipt["manifest_sha256"],
            "source_sha256": scan_receipt.get("source_sha256"),
            "final_diff_sha256": final_diff_sha,
            "authority_commit": authority_sha,
            "post_commit": post_sha,
        }
        if args.case == "egress-sender":
            if mutant4_receipt is None:
                raise RuntimeError("mutant4 authority receipt missing")
            result["mutant4_manifest_sha256"] = AUTHORITY_SHA256[AUTHORITY_FILES[0]]
            result["mutant4_fixture_replay"] = {
                "status": "passed",
                "preimage_sha256": mutant4_receipt["fixture_preimage_sha256"],
                "postimage_sha256": mutant4_receipt["fixture_postimage_sha256"],
            }
            result["mutant4_authority_files"] = mutant4_receipt["authority_files"]
            result["mutant4_patches"] = [
                identity
                for identity in mutant4_receipt["authority_files"]
                if identity["name"].endswith(".patch")
            ]
        print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    finally:
        shutil.rmtree(scratch_parent, ignore_errors=True)


if __name__ == "__main__":
    main()
