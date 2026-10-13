#!/usr/bin/env python3
"""Build and compare Nitro EIF artifacts without a local amd64 client."""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile
import uuid

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
BUILDKIT = "moby/buildkit:v0.23.2@sha256:e39f6119f134b4811af19fd5c20f495a6a264a85c1b6920daf569b23009dd42c"
EXCLUDED_ROOTS = {".agents", ".github", ".depot", "docs"}
EXCLUDED_FILES = {
    "llms.txt", "llms-full.txt", "etc/docs-index.toml",
    "etc/docker/nitro-enclave/baseline.json",
}
ARTIFACTS = (
    "base-prover-nitro-enclave", "bootstrap/bzImage", "bootstrap/bzImage.config",
    "cmdline", "eif_build", "init-ramdisk-initrd.img", "user-ramdisk-initrd.img", "eif.bin",
)


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def context(ref, output, root=ROOT):
    tree = subprocess.check_output(["git", "rev-parse", f"{ref}^{{tree}}"], cwd=root, text=True).strip()
    process = subprocess.Popen(["git", "archive", "--format=tar", tree], cwd=root, stdout=subprocess.PIPE)
    with process.stdout, tarfile.open(fileobj=process.stdout, mode="r|") as source, tarfile.open(output, "w", format=tarfile.USTAR_FORMAT) as target:
        for member in source:
            path = PurePosixPath(member.name)
            if path.parts[0] in EXCLUDED_ROOTS or member.name in EXCLUDED_FILES or member.name.endswith(".md"):
                continue
            if path.is_absolute() or ".." in path.parts or not (member.isfile() or member.isdir() or member.issym()):
                raise ValueError(f"Unsupported source entry: {member.name}")
            member.uid = member.gid = member.mtime = 0
            member.uname = member.gname = ""
            member.pax_headers = {}
            member.mode = 0o755 if member.isdir() or member.mode & 0o111 else 0o644
            target.addfile(member, source.extractfile(member) if member.isfile() else None)
    if process.wait() != 0:
        raise RuntimeError("git archive failed")
    return {"source_tree": tree, "source_identity": digest(output)}


def build(ref, output):
    output = Path(output).resolve()
    if output.exists() and any(output.iterdir()):
        raise ValueError(f"Output must be absent or empty: {output}")
    host = json.loads(subprocess.check_output(["docker", "info", "--format", "{{json .}}"], text=True))
    if host["OSType"] != "linux" or host["Architecture"] not in {"x86_64", "amd64"}:
        raise ValueError("Select a native Linux x86_64 Docker context. Local arm64 emulation is not supported.")
    builder = "nitro-reproduce-" + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory() as tmp:
        archive = Path(tmp) / "context.tar"
        identity = context(ref, archive)
        subprocess.run([
            "docker", "buildx", "create", "--name", builder, "--driver", "docker-container",
            "--driver-opt", f"image={BUILDKIT}",
        ], check=True)
        try:
            output.mkdir(parents=True, exist_ok=True)
            with archive.open("rb") as stream:
                subprocess.run([
                    "docker", "buildx", "build", "--builder", builder, "--platform", "linux/amd64",
                    "--no-cache", "--provenance=false", "--progress", "plain",
                    "-f", "etc/docker/Dockerfile.nitro-enclave", "--target", "artifacts",
                    "--metadata-file", str(output / "buildkit-metadata.json"),
                    "--output", f"type=local,dest={output}", "-",
                ], stdin=stream, check=True)
            write_json(output / "identity.json", identity)
            write_json(output / "host.json", {"docker_id": host["ID"], "name": host["Name"], "architecture": host["Architecture"], "buildkit": BUILDKIT})
            write_json(output / "manifest.json", manifest(output))
        finally:
            subprocess.run(["docker", "buildx", "rm", "--force", builder], check=True)
    print(json.dumps(manifest(output), indent=2, sort_keys=True))


def manifest(directory):
    directory = Path(directory)
    identity = json.loads((directory / "identity.json").read_text())
    measurements = json.loads((directory / "measurements.json").read_text())
    pcr0 = measurements["PCR0"]
    if not isinstance(pcr0, str) or not re.fullmatch(r"[0-9a-f]{96}", pcr0):
        raise ValueError("Invalid or missing PCR0")
    return {
        "source_identity": identity["source_identity"],
        "eif_sha256": digest(directory / "eif.bin"), "pcr0": pcr0,
        "sha256": {name: digest(directory / name) for name in ARTIFACTS},
    }


def compare(first, second, baseline=None, require_independent=False):
    first, second = Path(first), Path(second)
    left, right = manifest(first), manifest(second)
    differences = []
    for key in ("source_identity", "eif_sha256", "pcr0"):
        if left[key] != right[key]:
            differences.append(f"{key}: {left[key]} != {right[key]}")
    for name in ARTIFACTS:
        if left["sha256"][name] != right["sha256"][name]:
            differences.append(f"{name}: {left['sha256'][name]} != {right['sha256'][name]}")
    with (first / "eif.bin").open("rb") as a, (second / "eif.bin").open("rb") as b:
        while True:
            chunk = a.read(1024 * 1024)
            if chunk != b.read(1024 * 1024):
                differences.append("Full EIF bytes differ")
                break
            if not chunk:
                break
    if require_independent:
        hosts = [json.loads((directory / "host.json").read_text()) for directory in (first, second)]
        if not hosts[0]["docker_id"] or hosts[0]["docker_id"] == hosts[1]["docker_id"]:
            differences.append("Independent Docker hosts were not demonstrated")
        if any(host["architecture"] not in {"amd64", "x86_64"} or host["buildkit"] != BUILDKIT for host in hosts):
            differences.append("Noncanonical build host or BuildKit pin")
    if baseline is not None:
        expected = json.loads(Path(baseline).read_text())
        if expected["measurements"] != left:
            differences.append("Checked-in baseline differs. Review diagnostics and follow the measured baseline update policy.")
    if differences:
        raise ValueError("\n".join(differences))
    return left


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("context", "build"):
        subparser = commands.add_parser(command)
        subparser.add_argument("--ref", default="HEAD")
        subparser.add_argument("--output", type=Path, required=True)
    for command in ("compare", "baseline"):
        subparser = commands.add_parser(command)
        subparser.add_argument("first", type=Path)
        subparser.add_argument("second", type=Path)
        subparser.add_argument("--require-independent", action="store_true")
        if command == "compare":
            subparser.add_argument("--baseline", type=Path)
    args = parser.parse_args()
    if args.command == "context":
        print(json.dumps(context(args.ref, args.output), indent=2, sort_keys=True))
    elif args.command == "build":
        build(args.ref, args.output)
    elif args.command == "compare":
        result = compare(args.first, args.second, args.baseline, args.require_independent)
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        result = compare(args.first, args.second, require_independent=True)
        print(json.dumps({"source_tree": json.loads((args.first / "identity.json").read_text())["source_tree"], "measurements": result}, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
