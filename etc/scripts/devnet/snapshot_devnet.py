#!/usr/bin/env python3
"""Non-destructive, mainnet-identity snapshot fork. Requires Python 3.11+, Docker and cast."""

import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess


PROTOCOL_VERSIONS = "0x7480Afc8D99a5c645c247dB5A1e4a4f440e6e095"
ROLES = ("sequencer", "validator")
OBSOLETE_CONFIG = {
    "fork_block": "init discovers F from the sequencer snapshot; remove it",
    "rollup_env": "no Base rollup endpoint is used; remove it",
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def number(value):
    return int(value, 16) if isinstance(value, str) and value.startswith("0x") else int(value)


def redact(text, secrets):
    for secret in secrets:
        if secret:
            text = text.replace(secret, "<redacted>")
    return re.sub(r"[A-Za-z][A-Za-z0-9+.-]*://\S+", "<url>", text)


def run(*args, env=None, timeout=120, secrets=None):
    # Commands may contain throwaway keys or provider credentials. Never echo argv. With `secrets`,
    # the tail of stderr is reported after removing those values and every URL.
    try:
        return subprocess.run(args, env=env, check=True, capture_output=True,
                              text=True, timeout=timeout).stdout.strip()
    except subprocess.CalledProcessError as error:
        detail = f": {redact(error.stderr[-2000:].strip(), secrets)}" if secrets is not None else ""
        raise RuntimeError(f"{Path(args[0]).name} failed{detail or '; inspect the local service logs'}") from error
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"{Path(args[0]).name} timed out after {timeout}s; data preserved") from error
    except (subprocess.SubprocessError, OSError) as error:
        raise RuntimeError(f"{Path(args[0]).name} failed; inspect the local service logs") from error


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    with open(temporary, "w", opener=lambda p, flags: os.open(p, flags, 0o600)) as output:
        json.dump(value, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    temporary.replace(path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def validate_paths(directory, paths):
    resolved = [directory.resolve(), *(Path(p).expanduser().resolve(strict=True) for p in paths)]
    for index, left in enumerate(resolved):
        for right in resolved[index + 1:]:
            require(not left.is_relative_to(right) and not right.is_relative_to(left),
                    "fork directory and datadirs must be distinct, non-nested paths")
    databases = [path / "db/mdbx.dat" for path in resolved[1:]]
    for path, database in zip(resolved[1:], databases):
        require(database.is_file(), f"not an existing Reth datadir: {path}")
        require(database.resolve().is_relative_to(path), f"database resolves outside its datadir: {path}")
    stats = [database.stat() for database in databases]
    # Device and inode identify one database even through bind-mount aliases.
    require(len({(stat.st_dev, stat.st_ino) for stat in stats}) == len(stats), "datadirs share one database")
    require(all(stat.st_nlink == 1 for stat in stats), "hard-linked databases are not independent writable copies")
    return resolved[1:]


class SnapshotFork:
    def __init__(self, directory):
        self.directory = Path(directory).expanduser().resolve()
        path = self.directory / "manifest.json"
        self.manifest = json.loads(path.read_text()) if path.exists() else None

    def save(self):
        write_json(self.directory / "manifest.json", self.manifest)

    def prepare(self, config):
        """Persists the fork's identity and keys; returns False once initialization has completed."""
        for field, reason in OBSOLETE_CONFIG.items():
            require(field not in config, f"{field} is obsolete: {reason}")
        allowed = {"sequencer_datadir", "validator_datadir", "base_image", "anvil_image", "batcher_image",
                   "port", "epoch_slots", "protocol_versions", "execution_env", "beacon_env"}
        require(not config.keys() - allowed, "unknown configuration fields")
        datadirs = validate_paths(self.directory, [config["sequencer_datadir"], config["validator_datadir"]])
        port = number(config.get("port", 19545))
        require(1024 <= port <= 65535, "L1 port must be unprivileged")
        images = {role: run("docker", "image", "inspect", "--format", "{{.Id}}", config[role + "_image"], secrets=())
                  for role in ("base", "anvil", "batcher")}
        require(all(re.fullmatch(r"sha256:[0-9a-f]{64}", image) for image in images.values()),
                "images must resolve to immutable local image IDs")
        settings = {
            "datadirs": dict(zip(ROLES, map(str, datadirs))), "images": images,
            "port": port, "epoch_slots": number(config.get("epoch_slots", 2)),
            "protocol_versions": config.get("protocol_versions", PROTOCOL_VERSIONS),
            "upstreams": {role: config.get(role + "_env", default) for role, default in (
                ("execution", "SNAPSHOT_UPSTREAM_EXECUTION"), ("beacon", "SNAPSHOT_UPSTREAM_BEACON"))},
        }
        require(settings["epoch_slots"] > 0, "epoch_slots must be positive")
        require(all(re.fullmatch(r"[A-Z_][A-Z0-9_]*", name) for name in settings["upstreams"].values()),
                "upstream references must be environment variable names, never URLs or credentials")
        require(re.fullmatch(r"0x[0-9a-fA-F]{40}", settings["protocol_versions"]), "invalid ProtocolVersions address")
        if self.manifest is not None:
            require(self.manifest["version"] == 2 and all(self.manifest[key] == value for key, value in settings.items()),
                    "initialization config changed; existing fork preserved")
            require(self.manifest["phase"] in ("inspecting", "prepared", "stopped", "starting", "running"),
                    "unsupported initialization phase")
            if self.manifest["phase"] != "inspecting":
                print("Fork already initialized; keeping its data and identities.", flush=True)
                return False
        else:
            self.manifest = {"version": 2, "project": "snapshot-" + secrets.token_hex(6), "phase": "inspecting",
                             **settings, "operations": {}}
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", port))
        self.manifest["setup_input"] = config
        self.save()
        for subdir in ("l1", "sequencer", "validator", "config"):
            (self.directory / subdir).mkdir(mode=0o700, exist_ok=True)
        keys_path = self.directory / "keys.json"
        require(keys_path.is_file() or "accounts" not in self.manifest, "existing fork keys are missing")
        if not keys_path.exists():
            write_json(keys_path, {role: "0x" + secrets.token_hex(32) for role in ("batcher", "signer", "user")})
        keys = json.loads(keys_path.read_text())
        self.manifest["accounts"] = {role: run("cast", "wallet", "address", "--private-key", key)
                                     for role, key in keys.items()}
        self.save()
        return True
