#!/usr/bin/env python3
"""Evidence-only writer for the B5-1a P0 registration raw seal.

Writes `base-mev/b5-cargo-registration-raw-seal/v1` from exactly eight named
raw capture inputs. This tool is never imported by any Rust crate, build
script, node, CLI runtime or production package. Every flag is mandatory,
accepted exactly once, and every repository-relative path, ref name and
target triple must byte-match the closed literal contract below; any
deviation, missing input, sidecar grammar violation, equality mismatch or
pre-existing output is a hard failure with no output written. The output
file is created with O_CREAT|O_EXCL and is excluded from its own preimage.
"""

import hashlib
import os
import re
import stat
import subprocess
import sys

SCHEMA = "base-mev/b5-cargo-registration-raw-seal/v1"
TARGET_TRIPLE = "x86_64-unknown-linux-gnu"
P0_PARENT_REF = "refs/gjc/b5-1a/p0-parent"
P0_REF = "refs/gjc/b5-1a/p0"

P0_PARENT_CLI = "target/b5-1a-cargo-history/p0-parent/raw/base-execution-cli.default.metadata.json"
P0_PARENT_NODE = "target/b5-1a-cargo-history/p0-parent/raw/base-reth-node.default.metadata.json"
P0_PARENT_LOCK = "target/b5-1a-cargo-history/p0-parent/raw/Cargo.lock"
P0_PARENT_LOCK_SIDECAR = "target/b5-1a-cargo-history/p0-parent/raw/Cargo.lock.sha256"
P0_CLI = "target/b5-1a-cargo-history/p0/raw/base-execution-cli.default.metadata.json"
P0_NODE = "target/b5-1a-cargo-history/p0/raw/base-reth-node.default.metadata.json"
P0_LOCK = "target/b5-1a-cargo-history/p0/raw/Cargo.lock"
P0_LOCK_SIDECAR = "target/b5-1a-cargo-history/p0/raw/Cargo.lock.sha256"
OUTPUT_PATH = "target/b5-1a-cargo-history/p0-registration-raw-seal-v1.json"

# Closed flag contract: flag name -> required literal value, except the explicit
# checkout root, which is environment-specific and validated separately. There
# is no default for any flag.
FLAG_LITERALS = {
    "--checkout-root": None,
    "--cargo-bin": "cargo",
    "--target": TARGET_TRIPLE,
    "--p0-parent-ref": P0_PARENT_REF,
    "--p0-ref": P0_REF,
    "--p0-parent-cli": P0_PARENT_CLI,
    "--p0-parent-node": P0_PARENT_NODE,
    "--p0-parent-lock": P0_PARENT_LOCK,
    "--p0-parent-lock-sidecar": P0_PARENT_LOCK_SIDECAR,
    "--p0-cli": P0_CLI,
    "--p0-node": P0_NODE,
    "--p0-lock": P0_LOCK,
    "--p0-lock-sidecar": P0_LOCK_SIDECAR,
    "--output": OUTPUT_PATH,
}

INPUT_PATHS = (
    P0_PARENT_CLI,
    P0_PARENT_NODE,
    P0_PARENT_LOCK,
    P0_PARENT_LOCK_SIDECAR,
    P0_CLI,
    P0_NODE,
    P0_LOCK,
    P0_LOCK_SIDECAR,
)

COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SIDECAR_RE = re.compile(rb"^([0-9a-f]{64})  ([^\n\0]+)\n$")


class SealError(Exception):
    """Raised for any contract violation; the writer then exits nonzero."""


def fail(message):
    raise SealError(message)


def parse_argv(argv):
    """Strict flag parsing: every known flag exactly once, nothing else."""
    values = {}
    index = 0
    while index < len(argv):
        flag = argv[index]
        if flag not in FLAG_LITERALS:
            fail(f"unknown or positional argument: {flag!r}")
        if flag in values:
            fail(f"duplicate flag: {flag}")
        if index + 1 >= len(argv):
            fail(f"flag missing value: {flag}")
        values[flag] = argv[index + 1]
        index += 2
    for flag, literal in FLAG_LITERALS.items():
        if flag not in values:
            fail(f"omitted mandatory flag: {flag}")
        if literal is not None and values[flag] != literal:
            fail(f"flag {flag} value {values[flag]!r} is not the closed literal {literal!r}")
    return values


def jcs_string(value):
    out = ['"']
    for ch in value:
        code = ord(ch)
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif code >= 0x20:
            out.append(ch)
        elif ch == "\b":
            out.append("\\b")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\f":
            out.append("\\f")
        elif ch == "\r":
            out.append("\\r")
        else:
            out.append(f"\\u{code:04x}")
    out.append('"')
    return "".join(out)


def jcs_serialize(value):
    """RFC 8785 JCS for the closed value domain used by this seal."""
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, str):
        return jcs_string(value)
    if isinstance(value, list):
        return "[" + ",".join(jcs_serialize(item) for item in value) + "]"
    if isinstance(value, dict):
        for key in value:
            if not isinstance(key, str):
                fail("non-string JSON object key")
        keys = sorted(value, key=lambda k: k.encode("utf-16-be"))
        return "{" + ",".join(jcs_string(k) + ":" + jcs_serialize(value[k]) for k in keys) + "}"
    fail(f"unsupported JSON value type: {type(value).__name__}")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def run_captured(argv, cwd, what):
    try:
        proc = subprocess.run(argv, cwd=cwd, capture_output=True, check=False)
    except OSError as error:
        fail(f"{what} failed to execute: {error}")
    if proc.returncode != 0:
        fail(f"{what} exited with status {proc.returncode}")
    return proc.stdout


def git_commit(checkout_root, spec, what):
    stdout = run_captured(
        ["git", "-C", checkout_root, "rev-parse", "--verify", spec], checkout_root, what
    )
    commit = stdout.decode("ascii", errors="strict").strip()
    if not COMMIT_RE.match(commit):
        fail(f"{what} did not resolve to a full lowercase 40-hex commit")
    return commit


def read_input_once(checkout_root, rel_path):
    components = rel_path.split("/")
    if not components or any(component in ("", ".", "..") for component in components):
        fail(f"input path is not a closed repository-relative path: {rel_path}")

    directory_fd = os.open(
        checkout_root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW
    )
    descriptor = None
    try:
        for component in components[:-1]:
            child_fd = os.open(
                component,
                os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW,
                dir_fd=directory_fd,
            )
            os.close(directory_fd)
            directory_fd = child_fd

        descriptor = os.open(
            components[-1],
            os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
            dir_fd=directory_fd,
        )
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            fail(f"input is not a regular file: {rel_path}")

        chunks = []
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                return b"".join(chunks)
            chunks.append(chunk)
    finally:
        if descriptor is not None:
            os.close(descriptor)
        os.close(directory_fd)


def file_binding(rel_path, data):
    return {"path": rel_path, "byte_len": str(len(data)), "sha256": sha256_hex(data)}


def parse_sidecar(sidecar_bytes, lock_rel_path, lock_bytes, what):
    match = SIDECAR_RE.match(sidecar_bytes)
    if match is None or match.end() != len(sidecar_bytes):
        fail(f"{what} is not exactly '<64-lower-hex><two spaces><lock path><LF>'")
    digest = match.group(1).decode("ascii")
    named_path = match.group(2).decode("utf-8", errors="strict")
    if named_path != lock_rel_path:
        fail(f"{what} names {named_path!r} instead of {lock_rel_path!r}")
    if digest != sha256_hex(lock_bytes):
        fail(f"{what} digest does not match its own lock bytes")
    return digest


def write_create_new(checkout_root, rel_path, payload):
    root = os.path.abspath(checkout_root)
    if os.path.realpath(root) != root:
        fail("checkout root resolves through a symlink")
    components = rel_path.split("/")
    if not components or any(component in ("", ".", "..") for component in components):
        fail("output path is not a closed repository-relative path")

    directory_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for component in components[:-1]:
            child_fd = os.open(
                component,
                os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW,
                dir_fd=directory_fd,
            )
            os.close(directory_fd)
            directory_fd = child_fd

        descriptor = os.open(
            components[-1],
            os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
            0o644,
            dir_fd=directory_fd,
        )
        created = True
        try:
            view = memoryview(payload)
            written = 0
            while written < len(view):
                count = os.write(descriptor, view[written:])
                if count <= 0:
                    fail("output write made no progress")
                written += count
            os.fsync(descriptor)
            os.close(descriptor)
            descriptor = None
        except BaseException:
            if descriptor is not None:
                os.close(descriptor)
            if created:
                os.unlink(components[-1], dir_fd=directory_fd)
            raise
    finally:
        os.close(directory_fd)


def build_seal(values):
    checkout_root = values["--checkout-root"]
    if not os.path.isabs(checkout_root):
        fail("--checkout-root must be an absolute path")
    if not os.path.isdir(checkout_root):
        fail("--checkout-root is not a directory")
    cargo_bin = values["--cargo-bin"]

    p0_parent_commit = git_commit(
        checkout_root, P0_PARENT_REF + "^{commit}", "p0-parent ref resolution"
    )
    p0_commit = git_commit(checkout_root, P0_REF + "^{commit}", "p0 ref resolution")
    parent_of_p0 = git_commit(checkout_root, p0_commit + "^^{commit}", "p0 parent resolution")
    if p0_parent_commit != parent_of_p0:
        fail("p0-parent ref does not equal the first parent of the p0 commit")

    cargo_stdout = run_captured(
        [cargo_bin, "version", "--verbose"], checkout_root, "cargo version --verbose"
    )

    contents = {}
    for rel_path in INPUT_PATHS:
        contents[rel_path] = read_input_once(checkout_root, rel_path)
    if OUTPUT_PATH in contents:
        fail("output path is listed as an input")

    parent_digest = parse_sidecar(
        contents[P0_PARENT_LOCK_SIDECAR],
        P0_PARENT_LOCK,
        contents[P0_PARENT_LOCK],
        "p0-parent lock sidecar",
    )
    p0_digest = parse_sidecar(
        contents[P0_LOCK_SIDECAR], P0_LOCK, contents[P0_LOCK], "p0 lock sidecar"
    )

    equality = {
        "cli_metadata_bytes": contents[P0_PARENT_CLI] == contents[P0_CLI],
        "node_metadata_bytes": contents[P0_PARENT_NODE] == contents[P0_NODE],
        "lock_bytes": contents[P0_PARENT_LOCK] == contents[P0_LOCK],
        "parsed_lock_digest": parent_digest == p0_digest,
    }
    for name, held in equality.items():
        if held is not True:
            fail(f"equality check failed: {name}")

    seal = {
        "schema": SCHEMA,
        "cargo_version_sha256": sha256_hex(cargo_stdout),
        "target": TARGET_TRIPLE,
        "refs": {
            "p0_parent": {"name": P0_PARENT_REF, "commit": p0_parent_commit},
            "p0": {"name": P0_REF, "commit": p0_commit},
            "p0_parent_is_p0_parent": True,
        },
        "inputs": [
            file_binding(rel_path, contents[rel_path]) for rel_path in sorted(INPUT_PATHS)
        ],
        "equality": equality,
        "verdict": "PASS",
    }
    return checkout_root, jcs_serialize(seal).encode("utf-8")


def main(argv):
    try:
        values = parse_argv(argv)
        checkout_root, payload = build_seal(values)
        write_create_new(checkout_root, OUTPUT_PATH, payload)
    except (SealError, OSError, UnicodeError) as error:
        print(f"b5_write_registration_raw_seal: error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
