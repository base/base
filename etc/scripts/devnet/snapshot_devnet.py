#!/usr/bin/env python3
"""Non-destructive, mainnet-identity snapshot fork. Requires Python 3.11+, Docker and cast."""

import argparse
import fcntl
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parents[3]
COMPOSE = ROOT / "etc/docker/docker-compose.snapshot.yml"
DEFAULT_TIMEOUT = 7200
PROTOCOL_VERSIONS = "0x7480Afc8D99a5c645c247dB5A1e4a4f440e6e095"
IMPLEMENTATION_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
COBALT_ID = 12
UPGRADES = (
    "regolith", "canyon", "delta", "ecotone", "fjord", "granite", "holocene",
    "pectra_blob_schedule", "isthmus", "jovian", "azul", "beryl", "cobalt", "denim", "everest",
)
LEGACY_UPGRADE_COUNT = UPGRADES.index("azul")
READ_METHODS = {
    "eth_chainId", "eth_getBlockByNumber", "eth_getBlockByHash", "eth_call", "eth_getCode", "eth_getStorageAt",
}
ROLES = ("sequencer", "validator")
EXECUTION_RPC_PORT = 8545
CONSENSUS_RPC_PORT = 9545
GOSSIP_PORT = 9222
OBSOLETE_CONFIG = {
    "fork_block": "init discovers F from the sequencer snapshot; remove it",
    "rollup_env": "no Base rollup endpoint is used; remove it",
}


class Unavailable(RuntimeError):
    """A local or upstream service is not reachable yet; callers may poll again."""


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def number(value):
    try:
        return int(value, 16) if isinstance(value, str) and value.startswith("0x") else int(value)
    except ValueError:
        # Remote results may echo provider credentials; never repeat the value.
        raise ValueError("expected a decimal or 0x-prefixed integer (value redacted)") from None


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


def request_json(url, body=None, *, path=None):
    try:
        if path is not None:
            # Extend the endpoint's own path, keeping any provider query (such as an API key) last.
            parts = urllib.parse.urlsplit(url)
            url = parts._replace(path=parts.path.rstrip("/") + path).geturl()
        request = urllib.request.Request(
            url, data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "User-Agent": "base-snapshot-devnet"},
        )
        # Do not forward localhost traffic through an inherited HTTP proxy.
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(
            request, timeout=20
        ) as response:
            payload = response.read(32 * 1024 * 1024 + 1)
        require(len(payload) <= 32 * 1024 * 1024, "RPC response exceeds 32 MiB")
        return json.loads(payload)
    except (OSError, ValueError, http.client.HTTPException, urllib.error.URLError) as error:
        raise Unavailable("RPC/Beacon request failed (endpoint redacted)") from error


def rpc(url, method, *params, upstream=False):
    require(not upstream or method in READ_METHODS, "refusing an upstream write")
    response = request_json(url, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    if "error" in response:
        raise Unavailable(f"{method} failed (provider error redacted)")
    return response["result"]


def call(url, address, signature, *args, block="latest", upstream=False):
    data = run("cast", "calldata", signature, *map(str, args))
    encoded = rpc(url, "eth_call", {"to": address, "data": data}, block, upstream=upstream)
    return json.loads(run("cast", "abi-decode", "--json", signature, encoded))[0]


def wait(description, check, timeout, poll_interval=1, progress=None, report_interval=30, diagnostics=None):
    """Polls `check` until truthy.

    `timeout=None` waits without a deadline. Otherwise, without `progress`, `timeout` bounds the
    whole wait. With it, `timeout` bounds a stall: any
    change in the first element of `progress()` restarts it, and the second element, which must
    not contain endpoints or keys, is printed every `report_interval` seconds. The description is
    printed before the first poll and, without `progress`, with the elapsed time at each report.
    Optional `diagnostics` describes the first failed poll and each report, without extending waits.
    """
    print(f"waiting for {description}", file=sys.stderr, flush=True)
    started = time.monotonic()
    deadline = None if timeout is None else started + timeout
    next_report, state, message = started + report_interval, None, ""
    if diagnostics is not None:
        next_report = started
    while True:
        result = check()
        if result:
            return result
        now = time.monotonic()
        if progress is not None:
            current, message = progress()
            if current != state:
                state = current
                if timeout is not None:
                    deadline = now + timeout
        if now >= next_report:
            detail = message if progress is not None else f"{int(now - started)}s elapsed"
            if diagnostics is not None:
                if deadline is not None:
                    detail += f" ({max(0, int(deadline - now))}s remaining)"
                detail += f"; {diagnostics()}"
            print(f"waiting for {description}: {detail}", file=sys.stderr, flush=True)
            next_report = now + report_interval
        stalled = f" (no progress for {timeout}s; {message})" if progress is not None else ""
        require(deadline is None or now < deadline,
                f"timed out: {description}{stalled}; data preserved, rerun start to resume")
        time.sleep(poll_interval)


def next_slot(genesis, duration, tip, now):
    require(duration > 0 and tip >= genesis, "invalid Beacon clock")
    # Ceiling of wall clock; a restored head may already be slightly ahead of it.
    slot = max((now - genesis + duration - 1) // duration, (tip - genesis) // duration + 1)
    return genesis + slot * duration


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


def private_address(project, services, containers):
    """Returns the service and internal-network IPv4 of the one running project container among `services`.

    Only the Docker host can route to it, and it changes when the container is recreated.
    """
    matches = [container for container in containers if container["State"]["Running"]
               and container["Config"]["Labels"].get("com.docker.compose.project") == project
               and container["Config"]["Labels"].get("com.docker.compose.service") in services]
    if not matches:
        raise Unavailable(f"{' or '.join(services)} is not running")
    require(len(matches) == 1, f"ambiguous running containers for {' / '.join(services)}; "
            "inspection and production nodes must never share a datadir, stop the project and retry")
    network = project + "_private"
    networks = matches[0]["NetworkSettings"]["Networks"] or {}
    require(set(networks) == {network}, "L2 containers must be attached only to the project's internal network")
    try:
        address = ipaddress.ip_address(networks[network]["IPAddress"])
    except ValueError as error:
        raise RuntimeError("container has no IPv4 address on the project's internal network") from error
    require(address.version == 4 and address.is_private, "unexpected address on the project's internal network")
    return matches[0]["Config"]["Labels"]["com.docker.compose.service"], str(address)


def validate_fork(fork, header, finalized, genesis_time, slot_seconds):
    """Checks the discovered fork BlockInfo against the canonical upstream header."""
    require(header is not None and number(header["number"]) == fork["number"]
            and header["hash"].lower() == fork["hash"].lower()
            and header["parentHash"].lower() == fork["parentHash"].lower()
            and number(header["timestamp"]) == fork["timestamp"],
            "discovered fork block is not canonical upstream")
    require(fork["number"] <= number(finalized["number"]),
            "discovered fork block is not finalized yet; retry init after finality")
    require(slot_seconds > 0 and fork["timestamp"] >= genesis_time
            and (fork["timestamp"] - genesis_time) % slot_seconds == 0,
            "fork timestamp is not on the upstream Beacon slot grid")


def validate_boundary(inspections, fork_number):
    first, second = inspections
    require(first["chain_id"] == second["chain_id"] == 8453, "expected Base mainnet snapshots")
    require(first["rollup_config"] == second["rollup_config"], "snapshot configs differ")
    require(first["latest"] == second["latest"], "snapshot heads or system configs differ")
    for snapshot in inspections:
        for label in ("latest", "safe", "finalized"):
            require(snapshot[label]["block_info"]["l1origin"]["number"] <= fork_number,
                    "snapshot L1 origin is after the fork")


def derived_boundary(statuses, fork_number, initial_latest):
    """Returns the common derived safe head once every node has moved its L1 origin past F.

    `current_l1 == F` means the pipeline entered F, not that F's batches were applied; it only
    advances after the engine acknowledged every attribute derived from F.
    """
    if any(number(status["current_l1"]["number"]) <= fork_number for status in statuses.values()):
        return None
    for role, status in statuses.items():
        safe, unsafe = status["safe_l2"], status["unsafe_l2"]
        require(safe["hash"] == unsafe["hash"] and number(safe["number"]) == number(unsafe["number"]),
                f"{role} kept unsafe blocks after deriving F: the snapshot tail is not covered by canonical batches")
        require(number(safe["number"]) >= initial_latest["number"],
                f"{role} derived safe head is below the snapshot head after deriving F")
    heads = {(number(status["safe_l2"]["number"]), status["safe_l2"]["hash"]) for status in statuses.values()}
    require(len(heads) == 1, "sequencer and validator derived different safe heads")
    (height, block_hash), = heads
    return {"number": height, "hash": block_hash}


def upgrade_times(config):
    return [config.get(upgrade + "_time") if index < LEGACY_UPGRADE_COUNT else config.get("base", {}).get(upgrade)
            for index, upgrade in enumerate(UPGRADES)]


def validate_schedule(config, schedule, head_timestamp):
    require(COBALT_ID < len(schedule) <= len(UPGRADES), "unsupported ProtocolVersions layout")
    for index, activation in enumerate(upgrade_times(config)):
        expected = config["genesis"]["l2_time"] if activation == 0 else activation
        actual = schedule[index] if index < len(schedule) else 0
        if (expected is not None and expected <= head_timestamp) or (0 < actual <= head_timestamp):
            require(actual == expected,
                    f"contract would change historical {UPGRADES[index]} activation")


def validate_origins(inspections, url):
    for inspection in inspections:
        for label in ("latest", "safe", "finalized"):
            origin = inspection[label]["block_info"]["l1origin"]
            header = rpc(url, "eth_getBlockByNumber", hex(origin["number"]), False, upstream=True)
            require(header and header["hash"] == origin["hash"], "snapshot has a noncanonical L1 origin")


class SnapshotFork:
    def __init__(self, directory, timeout=DEFAULT_TIMEOUT):
        self.directory = Path(directory).expanduser().resolve()
        self.timeout = timeout
        path = self.directory / "manifest.json"
        self.manifest = json.loads(path.read_text()) if path.exists() else None
        self._containers = None

    def save(self):
        write_json(self.directory / "manifest.json", self.manifest)

    def containers(self):
        """This project's container records, cached until the next compose command."""
        if self._containers is None:
            ids = run("docker", "ps", "--all", "--quiet", "--no-trunc",
                      "--filter", f"label=com.docker.compose.project={self.manifest['project']}").split()
            self._containers = json.loads(run("docker", "inspect", *ids)) if ids else []
        return self._containers

    def running_services(self):
        return {container["Config"]["Labels"].get("com.docker.compose.service")
                for container in self.containers() if container["State"]["Running"]}

    def url(self, role):
        """L1 uses its loopback-published port. Docker ignores published ports of internal-only
        networks, so L2 RPCs use the container's internal-network IP, reachable only from this host."""
        if role == "l1":
            return f"http://127.0.0.1:{self.manifest['port']}"
        node, consensus = role.removesuffix("-cl"), role.endswith("-cl")
        # Both kinds running at once is ambiguous; inspection nodes only expose execution RPC.
        service, address = private_address(self.manifest["project"], ("inspect-" + node, node), self.containers())
        if consensus and service != node:
            raise Unavailable(f"{node} consensus RPC is not running")
        return f"http://{address}:{CONSENSUS_RPC_PORT if consensus else EXECUTION_RPC_PORT}"

    def endpoint(self, name):
        variable = self.manifest["upstreams"][name]
        value = os.environ.get(variable, "")
        require(value.startswith(("http://", "https://")), f"set {variable} to an HTTP(S) endpoint")
        return value

    def compose_env(self):
        manifest = self.manifest
        values = {"DIR": self.directory, "UID": os.getuid(), "GID": os.getgid(),
                  "BASE_IMAGE": manifest["images"]["base"]}
        for role in ROLES:
            values[role.upper() + "_DATADIR"] = manifest["datadirs"][role]
        return {**os.environ, **{"SNAPSHOT_" + key: str(value) for key, value in values.items()}}

    def compose(self, *args):
        # Announce lifecycle actions only; Compose output and its environment may carry credentials.
        for action, verb in (("up", "Starting"), ("stop", "Stopping")):
            if action in args:
                services = [arg for arg in args[args.index(action) + 1:] if not arg.startswith("-")]
                print(f"{verb} containers: {', '.join(services)}", file=sys.stderr, flush=True)
        self._containers = None
        return run("docker", "compose", "--project-name", self.manifest["project"],
                   "--file", str(COMPOSE), *args, env=self.compose_env(), timeout=self.timeout)

    def rpc_startup_status(self, role):
        """Summarizes current-run startup logs using only known stages and numeric progress fields."""
        service = role
        try:
            matches = [item for item in self.containers()
                       if item["Config"]["Labels"]["com.docker.compose.service"] in (role, "inspect-" + role)]
            matches = [item for item in matches if item["State"]["Running"]] or matches
            if len(matches) != 1:
                return f"{role}: no unique container found; check snapshot status"
            item = matches[0]
            service = item["Config"]["Labels"]["com.docker.compose.service"]
            if not item["State"]["Running"]:
                return f"{service}: exited (code {item['State']['ExitCode']}); inspect container logs"
            if role == "l1":
                return "l1: container running, Anvil RPC not ready"
            result = subprocess.run(
                ["docker", "logs", "--since", item["State"]["StartedAt"], "--tail", "50", item["Id"]],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, check=True, timeout=5)
            stages = {
                "StoragesHistory:": "repairing storage-history indexes",
                "AccountsHistory:": "repairing account-history indexes",
                "Collecting indices": "rebuilding history indexes",
                "Writing indices": "writing history indexes",
                "Healing static file inconsistencies": "repairing snapshot consistency",
                "Opening database": "opening snapshot database",
            }
            # Never forward raw logs: provider URLs, keys, or tokens may appear even on progress lines.
            for line in reversed(re.sub(r"\x1b\[[0-9;]*m", "", result.stdout).splitlines()):
                stage = next((label for marker, label in stages.items() if marker in line), None)
                if stage is None:
                    continue
                fields = dict(re.findall(
                    r"\b(batch_num|total_batches|batch_start|batch_end|current_block|processed_blocks|progress|checkpoint|target)="
                    r"(\d+(?:\.\d+)?%?)", line))
                if "batch_num" in fields and "total_batches" in fields:
                    stage += f"; batch {fields.pop('batch_num')}/{fields.pop('total_batches')} started"
                if fields:
                    stage += "; " + ", ".join(f"{key}={value}" for key, value in fields.items())
                timestamp = re.match(r"\d{4}-\d{2}-\d{2}T[\d:.]+Z", line)
                return f"{service}: last startup log" + (f" {timestamp[0]}" if timestamp else "") + f": {stage}"
            return f"{service}: container running; no recognized startup progress in recent logs"
        except (RuntimeError, OSError, subprocess.SubprocessError):
            return f"{service}: startup logs unavailable; still waiting for RPC"

    def await_rpc(self, role):
        def ready():
            try:
                return rpc(self.url(role), "eth_chainId")
            except Unavailable:
                self._containers = None
                require(self.running_services() & {role, "inspect-" + role},
                        f"{role} execution container exited or is missing; inspect container logs; data preserved")
                return False
        # Snapshot index repair can take hours before RPC is available. Only container failure or
        # operator cancellation ends this wait; individual RPC and Docker calls remain bounded.
        wait(f"{role} execution RPC", ready, None if role in ROLES else self.timeout, poll_interval=5,
             diagnostics=lambda: self.rpc_startup_status(role))

    def inspect(self, discover=False):
        """Inspects both datadirs; with `discover`, the sequencer inspection also finds F."""
        inspector = os.environ.get("BASE_SNAPSHOT_INSPECTOR", str(ROOT / "target/debug/base-devnet"))
        result = []
        require(not self.running_services() & set(ROLES),
                "production nodes are running on these datadirs; stop them before inspection")
        try:
            run(inspector, "inspect-snapshot", "--help")
        except RuntimeError as error:
            raise RuntimeError(f"snapshot inspector {inspector} is missing or lacks inspect-snapshot; build it with "
                               "`cargo build --locked -p base-system-tests --bin base-devnet` or set "
                               "BASE_SNAPSHOT_INSPECTOR") from error
        try:
            self.compose("--profile", "inspect", "up", "-d", "--no-build", "inspect-sequencer", "inspect-validator")
            for role in ROLES:
                self.await_rpc(role)
                command, env, timeout, secrets = [inspector, "inspect-snapshot", "--rpc-url",
                                                  self.url(role)], None, self.timeout, ()
                if discover and role == "sequencer":
                    # The inspector reads the standard names; config may name other variables.
                    upstreams = {"SNAPSHOT_UPSTREAM_" + name.upper(): self.endpoint(name)
                                 for name in ("execution", "beacon")}
                    env, secrets = {**os.environ, **upstreams}, tuple(upstreams.values())
                    command += ["--find-fork", "--timeout", str(self.timeout)]
                    timeout = self.timeout + 60  # Let the inspector report its own timeout first.
                print(f"Inspecting {role} snapshot" + (" and discovering L1 fork block F" if env else ""),
                      file=sys.stderr, flush=True)
                inspection = json.loads(run(*command, env=env, timeout=timeout, secrets=secrets))
                require(not env or "fork" in inspection, "inspector did not report a fork block")
                result.append(inspection)
        finally:
            self.compose("--profile", "inspect", "stop", "inspect-sequencer", "inspect-validator")
        return result

    def prepare(self, config):
        """Persists the fork's identity and keys; returns False once initialization has completed.

        Does not lock: the caller must hold the fork directory's exclusive `.lock`, as `main` does.
        """
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

    def initialize(self, config, allow_write):
        """Prepares, inspects and validates the fork, marking it `prepared` only after every check.

        Does not lock: the caller must hold the fork directory's exclusive `.lock`, as `main` does.
        """
        require(allow_write, "init requires --allow-write for the two disposable working datadirs")
        if not self.prepare(config):
            return
        execution, beacon = self.endpoint("execution"), self.endpoint("beacon")
        require(number(rpc(execution, "eth_chainId", upstream=True)) == 1, "expected Ethereum mainnet upstream")
        genesis = request_json(beacon, path="/eth/v1/beacon/genesis")["data"]
        duration = number(request_json(beacon, path="/eth/v1/config/spec")["data"]["SECONDS_PER_SLOT"])
        require(self.manifest.get("beacon_genesis", genesis) == genesis
                and self.manifest.get("slot_seconds", duration) == duration, "upstream Beacon identity changed")
        self.manifest.update(beacon_genesis=genesis, slot_seconds=duration)
        self.save()
        # F is the L1 block whose canonical batches complete the snapshot's (safe, latest] tail.
        inspections = self.inspect(discover=True)
        discovered = inspections[0].pop("fork")
        header = rpc(execution, "eth_getBlockByNumber", hex(discovered["number"]), False, upstream=True)
        finalized = rpc(execution, "eth_getBlockByNumber", "finalized", False, upstream=True)
        validate_fork(discovered, header, finalized, number(genesis["genesis_time"]), duration)
        fork = {key: header[key] for key in ("number", "hash", "timestamp", "parentHash")}
        validate_boundary(inspections, discovered["number"])
        config = inspections[0]["rollup_config"]
        validate_origins(inspections, execution)
        contract = self.manifest["protocol_versions"]
        schedule = list(map(number, call(execution, contract, "getSchedule()(uint64[])",
                                         block=fork["number"], upstream=True)))
        require(number(call(execution, contract, "minimumProtocolVersion()(uint256)",
                            block=fork["number"], upstream=True)) > 0,
                "ProtocolVersions must have a nonzero minimum protocol version")
        validate_schedule(config, schedule, inspections[0]["latest"]["block_info"]["timestamp"])
        system = config["l1_system_config_address"]
        portal = call(execution, system, "optimismPortal()(address)", block=fork["number"], upstream=True)
        owners = {system: call(execution, system, "owner()(address)", block=fork["number"], upstream=True),
                  contract: call(execution, contract, "proxyAdminOwner()(address)", block=fork["number"], upstream=True)}
        implementations = {}
        for address in (system, contract, portal):
            require(rpc(execution, "eth_getCode", address, fork["number"], upstream=True) != "0x",
                    "required contract has no code at the fork boundary")
            implementations[address] = rpc(execution, "eth_getStorageAt", address, IMPLEMENTATION_SLOT,
                                           fork["number"], upstream=True)
        write_json(self.directory / "config/rollup.json", config)
        self.manifest.update(phase="prepared", fork=fork, initial=inspections, schedule=schedule,
                             system_config=system, portal=portal, contracts=implementations, owners=owners)
        self.save()
        print(f"Prepared with L1 fork block {number(fork['number'])}; no local contracts changed.")


    def assert_local_l1(self):
        metadata = rpc(self.url("l1"), "anvil_metadata")
        fork = metadata.get("forkedNetwork") or {}
        require(number(metadata["chainId"]) == 1 and fork.get("forkBlockHash") == self.manifest["fork"]["hash"]
                and number(fork["forkBlockNumber"]) == number(self.manifest["fork"]["number"]),
                "local Anvil fork identity does not match manifest")
        require(request_json(self.url("l1"), path="/eth/v1/beacon/genesis")["data"] == self.manifest["beacon_genesis"],
                "local Beacon identity does not match manifest")

    def mine(self):
        self.assert_local_l1()
        tip = rpc(self.url("l1"), "eth_getBlockByNumber", "latest", False)
        timestamp = next_slot(number(self.manifest["beacon_genesis"]["genesis_time"]),
                              self.manifest["slot_seconds"], number(tip["timestamp"]), int(time.time()))
        require(timestamp - time.time() <= self.manifest["slot_seconds"],
                "restored L1 is ahead of wall clock; wait before resuming")
        time.sleep(max(0, timestamp - time.time()))
        # Fork-aware Beacon mode aligns subsequent interval-mined blocks to this same grid,
        # skipping missed slots. Do not use parent+duration timestamps, which accumulate lag.
        rpc(self.url("l1"), "evm_mine", {"timestamp": timestamp})

    def send(self, name, sender, target, signature, *args, value=0):
        """Sends one local L1 transaction as `sender` exactly once, journaled under `name` in the manifest.

        The nonce is persisted before sending and the hash after, so a retry waits for the recorded
        transaction; a send interrupted before its hash was saved is ambiguous and is never resent.
        """
        self.assert_local_l1()
        operations = self.manifest["operations"]
        operation = operations.get(name)
        transaction = {"from": sender, "to": target, "value": hex(value),
                       "data": run("cast", "calldata", signature, *map(str, args))}
        if operation:
            require(all(operation["transaction"][key] == value for key, value in transaction.items()),
                    f"{name} already records a different operation")
            require("hash" in operation, f"{name} submission was interrupted; reconcile its nonce manually before retrying")
            transaction_hash = operation["hash"]
        else:
            transaction["nonce"] = rpc(self.url("l1"), "eth_getTransactionCount", sender, "pending")
            rpc(self.url("l1"), "anvil_impersonateAccount", sender)
            try:
                rpc(self.url("l1"), "anvil_setBalance", sender, hex(100 * 10**18))
                transaction["gas"] = rpc(self.url("l1"), "eth_estimateGas", transaction)
                operations[name] = {"transaction": transaction}
                self.save()  # Persist the nonce before the potentially ambiguous send.
                transaction_hash = rpc(self.url("l1"), "eth_sendTransaction", transaction)
                operations[name]["hash"] = transaction_hash
                self.save()
            finally:
                rpc(self.url("l1"), "anvil_stopImpersonatingAccount", sender)
        if self.manifest["phase"] != "running" and rpc(self.url("l1"), "eth_getTransactionReceipt", transaction_hash) is None:
            self.mine()
        receipt = wait(name + " receipt", lambda: rpc(self.url("l1"), "eth_getTransactionReceipt", transaction_hash), self.timeout)
        operations[name]["receipt"] = receipt
        self.save()
        require(number(receipt["status"]) == 1, f"{name} reverted; inspect its persisted receipt")
        return receipt

    def bootstrap(self):
        """Authorizes this fork's batcher and unsafe-block signer on the local SystemConfig as its owner."""
        url = self.url("l1")
        system = self.manifest["system_config"]
        owner = call(url, system, "owner()(address)")
        batcher = self.manifest["accounts"]["batcher"]
        signer = self.manifest["accounts"]["signer"]
        self.send("set-batcher", owner, system, "setBatcherHash(bytes32)", "0x" + batcher[2:].zfill(64))
        self.send("set-signer", owner, system, "setUnsafeBlockSigner(address)", signer)
        require(number(call(url, system, "batcherHash()(bytes32)")) == number(batcher), "batcher update not applied")
        require(call(url, system, "unsafeBlockSigner()(address)").lower() == signer.lower(), "signer update not applied")
        rpc(url, "anvil_setBalance", batcher, hex(100 * 10**18))

    def sync_status(self, role):
        return rpc(self.url(role + "-cl"), "optimism_syncStatus")

    def consensus_ready(self, role):
        """False while the role is stopped or starting; identity/safety failures still raise."""
        try:
            return bool(self.sync_status(role))
        except Unavailable:
            return False

    def wait_upgrades(self):
        expected = [timestamp or None for timestamp in self.manifest["schedule"]]
        for role in ("sequencer", "validator"):
            def observed():
                config = rpc(self.url(role + "-cl"), "optimism_rollupConfig")
                ready = rpc(self.url(role + "-cl"), "base_upgradeReadiness")
                return ready["ready"] and upgrade_times(config)[:len(expected)] == expected
            wait(role + " observing the recorded upgrade schedule", observed, self.timeout)

    def wait_boundary(self):
        """Gates sequencing on both nodes having derived every canonical batch through F."""
        fork = self.manifest["fork"]
        fork_number = number(fork["number"])
        initial = self.manifest["initial"][0]["latest"]["block_info"]
        if number(rpc(self.url("l1"), "eth_blockNumber")) == fork_number:
            # Nodes can only report leaving F once a local successor exists. One wall-time slot is enough.
            self.mine()
        successor = rpc(self.url("l1"), "eth_getBlockByNumber", hex(fork_number + 1), False)
        require(successor and successor["parentHash"] == fork["hash"], "local L1 has no successor built on F")
        statuses = {}

        def derived():
            try:
                statuses.update((role, self.sync_status(role)) for role in ROLES)
            except Unavailable:
                return None
            return derived_boundary(statuses, fork_number, initial)

        def progress():
            heads = {role: (status["current_l1"]["number"], status["safe_l2"]["number"], status["unsafe_l2"]["number"])
                     for role, status in statuses.items()}
            return heads, "; ".join(f"{role} L1 {l1}/{fork_number + 1} needed, safe {safe}, unsafe {unsafe}"
                                    for role, (l1, safe, unsafe) in heads.items()) or "consensus status unavailable"

        safe = wait("both nodes deriving past L1 fork block F", derived, self.timeout, progress=progress)
        for role in ROLES:
            block = rpc(self.url(role), "eth_getBlockByNumber", hex(initial["number"]), False)
            require(block and block["hash"] == initial["hash"], f"snapshot head is no longer canonical on {role}")
        self.manifest.update(boundary_validated=True, boundary={
            "l1_successor": {key: successor[key] for key in ("number", "hash")},
            "current_l1": {role: statuses[role]["current_l1"] for role in ROLES}, "safe_l2": safe})
        self.save()

    def peers(self):
        infos = {role: rpc(self.url(role + "-cl"), "opp2p_self") for role in ROLES}
        for role, other in (("sequencer", "validator"), ("validator", "sequencer")):
            # Gossip stays on the internal network; resolve the peer's address there explicitly.
            _, ip = private_address(self.manifest["project"], (other,), self.containers())
            address = f"/ip4/{ip}/tcp/{GOSSIP_PORT}/p2p/{infos[other]['peerID']}"
            rpc(self.url(role + "-cl"), "opp2p_connectPeer", address)

    def require_batcher(self):
        self._containers = None
        require("batcher" in self.running_services(), "batcher exited after start (any exit, including "
                "code 0, fails); inspect its logs; data preserved, rerun start to resume")

    def start_batcher(self):
        """Starts the batcher and requires both nodes to derive one canonical block it batched."""
        target = max(number(self.sync_status(role)["safe_l2"]["number"]) for role in ROLES) + 1
        self.compose("up", "-d", "--no-build", "batcher")
        statuses = {}

        def batched():
            self.require_batcher()
            try:
                statuses.update((role, self.sync_status(role)) for role in ROLES)
            except Unavailable:
                return False
            return all(number(status["safe_l2"]["number"]) >= target for status in statuses.values())

        def progress():
            heads = {role: number(status["safe_l2"]["number"]) for role, status in statuses.items()}
            return heads, "; ".join(f"{role} safe {safe}/{target} needed"
                                    for role, safe in heads.items()) or "consensus status unavailable"

        wait("batcher advancing both safe heads", batched, self.timeout, progress=progress)
        hashes = {(rpc(self.url(role), "eth_getBlockByNumber", hex(target), False) or {}).get("hash")
                  for role in ROLES}
        require(len(hashes) == 1 and None not in hashes,
                f"sequencer and validator disagree on canonical safe block {target}; data preserved")

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    init = commands.add_parser("init", help="initialize or resume a fork from existing writable snapshot copies")
    init.add_argument("--dir", required=True, help="fork directory")
    init.add_argument("--config", required=True)
    init.add_argument("--allow-write", action="store_true")
    init.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT,
                      help="seconds per Compose command and snapshot inspection, including fork discovery "
                           "(default: %(default)s); execution RPC startup waits without a deadline")
    args = parser.parse_args()
    require(args.timeout > 0, "timeout must be positive")
    fork = SnapshotFork(args.dir, args.timeout)
    require(not fork.directory.exists() or fork.manifest is not None
            or {entry.name for entry in fork.directory.iterdir()} <= {".lock", "manifest.json.tmp"},
            "init directory contains unrecognized data; existing data is never overwritten")
    fork.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    with open(fork.directory / ".lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        # Re-read state persisted by the previous lock holder.
        fork = SnapshotFork(args.dir, args.timeout)
        fork.initialize(json.loads(Path(args.config).read_text()), args.allow_write)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("snapshot devnet: interrupted; data preserved, rerun the same command to continue", file=sys.stderr)
        sys.exit(130)
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        print(f"snapshot devnet: {error}", file=sys.stderr)
        sys.exit(1)
