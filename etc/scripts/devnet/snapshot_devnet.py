#!/usr/bin/env python3
"""Non-destructive, mainnet-identity snapshot fork. Requires Python 3.11+, Docker and cast."""

import argparse
import contextlib
import fcntl
import getpass
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parents[3]
COMPOSE = ROOT / "etc/docker/docker-compose.snapshot.yml"
DEFAULT_TIMEOUT = 1800
DEFAULT_DOWNLOAD_CONCURRENCY = 16
SNAPSHOT_INDEX = "https://chain.base.org/api/snapshots"
PROTOCOL_VERSIONS = "0x7480Afc8D99a5c645c247dB5A1e4a4f440e6e095"
IMPLEMENTATION_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
DENIM_ID = 13
COBALT_ID = 12
UPGRADES = (
    "regolith", "canyon", "delta", "ecotone", "fjord", "granite", "holocene",
    "pectra_blob_schedule", "isthmus", "jovian", "azul", "beryl", "cobalt", "denim", "everest",
)
READ_METHODS = {
    "eth_chainId", "eth_getBlockByNumber", "eth_getBlockByHash", "eth_call", "eth_getCode", "eth_getStorageAt",
}
ROLES = ("sequencer", "validator")
FORK_SERVICES = {"l1", *ROLES, "batcher"}
EXECUTION_RPC_PORT = 8545
CONSENSUS_RPC_PORT = 9545
GOSSIP_PORT = 9222
# The published full snapshot keeps 31 days of legacy 2s-block history (31 * 43200 blocks). The
# downloader's --full preset follows Denim production pruning, which retains 10x as many blocks.
PUBLISHED_FULL_SNAPSHOT_DISTANCE = 1339200
# Interpolated into Compose before F is known; invalid for Anvil and the nodes, which compose()
# also refuses to start until init records F.
UNKNOWN = "unknown-before-fork-discovery"
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


def request_json(url, body=None):
    request = urllib.request.Request(
        url, data=None if body is None else json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "User-Agent": "base-snapshot-devnet"},
    )
    try:
        # Do not forward localhost traffic through an inherited HTTP proxy.
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(
            request, timeout=20
        ) as response:
            payload = response.read(32 * 1024 * 1024 + 1)
        require(len(payload) <= 32 * 1024 * 1024, "RPC response exceeds 32 MiB")
        return json.loads(payload)
    except (OSError, ValueError, urllib.error.URLError) as error:
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


def wait(description, check, timeout, poll_interval=1, progress=None, report_interval=30):
    """Polls `check` until truthy.

    Without `progress`, `timeout` bounds the whole wait. With it, `timeout` bounds a stall: any
    change in the first element of `progress()` restarts it, and the second element, which must
    not contain endpoints or keys, is printed every `report_interval` seconds. The description is
    printed before the first poll and, without `progress`, with the elapsed time at each report.
    """
    print(f"waiting for {description}", file=sys.stderr, flush=True)
    started = time.monotonic()
    deadline, next_report, state, message = started + timeout, started + report_interval, None, ""
    while True:
        result = check()
        if result:
            return result
        now = time.monotonic()
        if progress is not None:
            current, message = progress()
            if current != state:
                state, deadline = current, now + timeout
        if now >= next_report:
            detail = message if progress is not None else f"{int(now - started)}s elapsed"
            print(f"waiting for {description}: {detail}", file=sys.stderr, flush=True)
            next_report = now + report_interval
        stalled = f" (no progress for {timeout}s; {message})" if progress is not None else ""
        require(now < deadline, f"timed out: {description}{stalled}; data preserved, rerun start to resume")
        time.sleep(poll_interval)


def next_slot(genesis, duration, tip, now):
    require(duration > 0 and tip >= genesis, "invalid Beacon clock")
    # Ceiling of wall clock; a restored head may already be slightly ahead of it.
    slot = max((now - genesis + duration - 1) // duration, (tip - genesis) // duration + 1)
    return genesis + slot * duration


def setup_path():
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "base/snapshot-devnet.json"


def configured_directory(directory=None):
    if directory is not None:
        return Path(directory).expanduser().resolve()
    path = setup_path()
    require(path.is_file(), "snapshot setup has not completed; run just devnet snapshot setup first")
    selected = json.loads(path.read_text()).get("directory")
    require(selected, "snapshot setup has not completed; run just devnet snapshot setup first")
    directory = Path(selected)
    require((directory / "manifest.json").is_file(),
            "saved snapshot directory is unavailable; run just devnet snapshot setup again")
    return directory


def setup_command(*args, lock_fd=None):
    # Downloads and builds can exceed the node-readiness timeout. Stream their progress rather
    # than retaining potentially hours of output; none of these commands contains RPC credentials.
    # A surviving child must retain the lock if its launcher is killed.
    require(subprocess.call(list(map(str, args)), cwd=ROOT,
                            pass_fds=() if lock_fd is None else (lock_fd,)) == 0,
            "snapshot setup command failed; downloaded data preserved")


def prepare_snapshot(args, fork, work, lock):
    """Resumes preparation under the fork lock; never copies after inspection begins."""
    journal = work / "setup.json"
    config_path = work / "input.json"
    state = json.loads(journal.read_text()) if journal.exists() else None
    config = json.loads(config_path.read_text()) if config_path.exists() else None
    if state is None:
        images = {"base": "base:local", "anvil": args.anvil_image, "batcher": args.batcher_image}
        phase = "build"
        if config is not None:
            # Older setup versions saved input.json but no completion marker. Never rerun
            # their unpinned downloader, which may select a different snapshot now.
            require((work / "builder/reth.toml").is_file(),
                    "unrecorded download may be incomplete; refusing to download over existing data")
            answer = input("Reuse the completed download? Confirm download succeeded, both datadirs "
                           "are unused, and no copy is running [y/N]: ").strip().lower()
            require(answer == "y", "existing data preserved; download completion was not confirmed")
            images = {role: config[role + "_image"] for role in images}
            phase = "copy"
        state = {"version": 1, "phase": phase, "images": images,
                 "download_container": "snapshot-download-" + secrets.token_hex(6)}
        write_json(journal, state)
    require(state["version"] == 1 and state["phase"] in ("build", "download", "copy", "initialize"),
            "unsupported snapshot setup journal; data preserved")
    require(state["phase"] == "build" or config is not None, "saved setup input is missing; data preserved")
    require(fork.manifest is None or state["phase"] == "initialize",
            "initialization already began; refusing to download or copy over its databases")
    require(not any((work / role).is_symlink() for role in ("builder", "validator")),
            "setup datadirs must not be symlinks")
    images = state["images"]
    if config is not None:
        require(config == {"sequencer_datadir": str(work / "builder"),
                           "validator_datadir": str(work / "validator"), "port": config["port"],
                           **{role + "_image": image for role, image in images.items()}},
                "saved setup input changed; refusing to overwrite data")
    require(shutil.which("docker") and shutil.which("cast") and shutil.which("rsync"),
            "setup requires Docker Compose, Foundry cast and rsync")
    if state["phase"] == "build":
        for role, image in images.items():
            if image.startswith("sha256:"):
                continue
            if role == "base":
                print("Building Base from the current checkout (including local changes).", flush=True)
                setup_command("docker", "buildx", "bake", "-f", "etc/docker/docker-bake.hcl", "base",
                              "--set", "base.args.PROFILE=release", "--load", lock_fd=lock.fileno())
            elif image.startswith("ghcr.io/"):
                setup_command("docker", "pull", image, lock_fd=lock.fileno())
            else:
                try:
                    run("docker", "image", "inspect", image)
                except RuntimeError:
                    if role == "anvil" and image == "base-anvil:snapshot-24ec5e47":
                        setup_command("just", "devnet", "snapshot", "build-anvil", lock_fd=lock.fileno())
                    elif role == "batcher" and image == "op-batcher:local":
                        setup_command("docker", "build", "-f", "etc/docker/Dockerfile.op-batcher", "-t", image, ".",
                                      lock_fd=lock.fileno())
                    else:
                        raise
            images[role] = run("docker", "image", "inspect", "--format", "{{.Id}}", image)
            write_json(journal, state)
        if "BASE_SNAPSHOT_INSPECTOR" in os.environ:
            require(Path(os.environ["BASE_SNAPSHOT_INSPECTOR"]).is_file(), "configured snapshot inspector does not exist")
        elif not state.get("inspector_built"):
            setup_command("cargo", "build", "--locked", "-p", "base-system-tests", "--bin", "base-devnet",
                          lock_fd=lock.fileno())
            state["inspector_built"] = True
            write_json(journal, state)
        if config is None:
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            config = {"sequencer_datadir": str(work / "builder"), "validator_datadir": str(work / "validator"),
                      "port": port, **{role + "_image": image for role, image in images.items()}}
            write_json(config_path, config)
        manifest_path = work / "download-manifest.json"
        if not manifest_path.exists():
            entries = [entry for entry in request_json(SNAPSHOT_INDEX)
                       if int(entry["chainId"]) == 8453 and entry["metadataUrl"].endswith("manifest.json")]
            require(entries, "no Base mainnet snapshot manifest available")
            latest = max(entries, key=lambda entry: int(entry["block"]))
            snapshot = request_json(latest["metadataUrl"])
            require(int(snapshot["chain_id"]) == 8453 and int(snapshot["block"]) == int(latest["block"]),
                    "snapshot manifest does not match the selected snapshot")
            snapshot["base_url"] = snapshot.get("base_url") or urllib.parse.urljoin(latest["metadataUrl"], ".")
            write_json(manifest_path, snapshot)
        state["phase"] = "download"
        write_json(journal, state)
    if state["phase"] == "download":
        require((work / "download-manifest.json").is_file(), "pinned download manifest is missing; data preserved")
        print("Downloading the pinned snapshot (completed files are verified and reused).", flush=True)
        setup_command("docker", "run", "--rm", "--name", state["download_container"],
                      "--user", f"{os.getuid()}:{os.getgid()}", "-e", "HOME=/work",
                      "-v", f"{work}:/work", "--entrypoint", "/app/base", images["base"],
                      "snapshot", "download", "--chain", "mainnet", "--datadir", "/work/builder",
                      "--manifest-path", "/work/download-manifest.json",
                      "--download-concurrency", str(args.download_concurrency),
                      "--with-txs-distance", str(PUBLISHED_FULL_SNAPSHOT_DISTANCE),
                      "--with-receipts-distance", str(PUBLISHED_FULL_SNAPSHOT_DISTANCE),
                      "--with-state-history-distance", str(PUBLISHED_FULL_SNAPSHOT_DISTANCE), "--non-interactive",
                      lock_fd=lock.fileno())
        state["phase"] = "copy"
        write_json(journal, state)
    if state["phase"] == "copy":
        validate_paths(fork.directory, [work / "builder"])
        print("Copying the downloaded datadir into an independent validator database.", flush=True)
        setup_command("rsync", "-a", "--info=progress2", "--no-inc-recursive",
                      f"{work / 'builder'}/", f"{work / 'validator'}/", lock_fd=lock.fileno())
        state["phase"] = "initialize"
        write_json(journal, state)
    print("Initializing the fork.", flush=True)
    fork.initialize(config, allow_write=True)


def setup(args):
    """Prepare or resume an experiment; completed setups only select the existing fork."""
    require(args.download_concurrency > 0, "download concurrency must be positive")
    selection = json.loads(setup_path().read_text()) if setup_path().exists() else {}
    work = None
    if not args.dir and not args.workdir:
        if selection.get("pending_directory"):
            pending = Path(selection["pending_directory"])
            if (pending / "manifest.json").is_file():
                args.dir = str(pending)
            else:
                args.workdir = str(pending.parent)
        elif selection.get("directory"):
            saved = Path(selection["directory"])
            if (saved / "manifest.json").is_file():
                args.dir = str(saved)
            else:
                require((saved / "setup.json").is_file() or (saved / "input.json").is_file(),
                        "saved snapshot directory is unavailable; select one with --workdir or --dir")
                args.workdir = str(saved)
    if args.dir:
        require(not args.workdir, "choose either --dir for an existing fork or --workdir for a setup")
        fork = SnapshotFork(args.dir, args.timeout)
        require(fork.manifest is not None, "--dir requires an initialized fork; use --workdir to resume setup")
    else:
        work = Path(args.workdir or input(f"Working directory [{Path.home() / 'data/snapshot-devnet'}]: ").strip()
                    or Path.home() / "data/snapshot-devnet").expanduser().resolve()
        require(not work.exists() or (work / "setup.json").is_file() or (work / "input.json").is_file()
                or (selection.get("pending_directory") == str(work / "fork")
                    and {entry.name for entry in work.iterdir()} <= {"fork"})
                or not any(work.iterdir()),
                "setup requires an unused working directory or a saved setup; existing data is never overwritten")
        fork = SnapshotFork(work / "fork", args.timeout)
    require(fork.manifest is None or (fork.manifest.get("version") == 2
            and fork.manifest.get("phase") in ("inspecting", "prepared", "stopped", "running", "starting")),
            "unsupported snapshot manifest; data preserved")

    defaults = {}
    env_file = setup_path().parent / "l1.env"
    if env_file.is_file():
        for line in env_file.read_text().splitlines():
            match = re.fullmatch(r"\s*(?:export\s+)?(ETH_L1_RPC|SNAPSHOT_UPSTREAM_EXECUTION|SNAPSHOT_UPSTREAM_BEACON)=(.*)", line)
            if match:
                values = shlex.split(match[2], comments=True)
                if len(values) == 1:
                    defaults[match[1]] = values[0]
    defaults.update(os.environ)
    variables = (fork.manifest or {}).get("upstreams", {
        "execution": "SNAPSHOT_UPSTREAM_EXECUTION", "beacon": "SNAPSHOT_UPSTREAM_BEACON"})
    execution = (defaults.get(variables["execution"]) or fork.credentials.get("execution")
                 or defaults.get("ETH_L1_RPC", ""))
    if not execution:
        execution = getpass.getpass("Ethereum L1 RPC (hidden): ").strip()
    require(execution.startswith(("http://", "https://")), "an HTTP(S) Ethereum L1 RPC is required")
    require(number(rpc(execution, "eth_chainId", upstream=True)) == 1, "expected Ethereum mainnet upstream")
    configured_beacon = defaults.get(variables["beacon"]) or fork.credentials.get("beacon")
    beacon = configured_beacon or execution
    try:
        genesis = request_json(beacon.rstrip("/") + "/eth/v1/beacon/genesis")["data"]
        require(number(genesis["genesis_time"]) > 0, "invalid Beacon genesis")
    except (Unavailable, KeyError, ValueError, RuntimeError):
        require(not configured_beacon, "configured Beacon endpoint failed validation; update "
                f"{variables['beacon']} or the fork's upstreams.json and rerun setup")
        beacon = getpass.getpass("Beacon API URL (the L1 endpoint did not provide a valid Beacon API): ").strip()
        require(beacon.startswith(("http://", "https://")), "an HTTP(S) Beacon endpoint is required")
        genesis = request_json(beacon.rstrip("/") + "/eth/v1/beacon/genesis")["data"]
        require(number(genesis["genesis_time"]) > 0, "invalid Beacon genesis")
    require(number(request_json(beacon.rstrip("/") + "/eth/v1/config/spec")["data"]["SECONDS_PER_SLOT"]) > 0,
            "invalid Beacon slot duration")
    fork.credentials = {"execution": execution.rstrip("/"), "beacon": beacon.rstrip("/")}
    # Make the selected endpoints available to initialization in this process too.
    os.environ.update({variables[role]: value for role, value in fork.credentials.items()})

    path = setup_path()
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fork.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    with open(fork.directory / ".lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        # Re-read after taking the same lock used by init/up/down.
        credentials = fork.credentials
        fork = SnapshotFork(fork.directory, args.timeout)
        fork.credentials = credentials
        write_json(fork.directory / "upstreams.json", credentials)
        if fork.manifest is None or fork.manifest["phase"] == "inspecting":
            write_json(path, {**selection, "pending_directory": str(fork.directory)})
            if work is None:
                require(fork.manifest is not None and "setup_input" in fork.manifest,
                        "use init with the original input config to resume this fork")
                fork.initialize(fork.manifest["setup_input"], allow_write=True)
            else:
                prepare_snapshot(args, fork, work, lock)
    write_json(path, {"directory": str(fork.directory)})
    print(f"Setup complete: {fork.directory}. Endpoint credentials saved privately in upstreams.json.\n"
          "Run just devnet snapshot up (then status/down); --dir is optional.")


def validate_paths(directory, paths):
    resolved = [directory.resolve(), *(Path(p).expanduser().resolve(strict=True) for p in paths)]
    for index, left in enumerate(resolved):
        for right in resolved[index + 1:]:
            require(not left.is_relative_to(right) and not right.is_relative_to(left),
                    "fork directory and datadirs must be distinct, non-nested paths")
    for path in resolved[1:]:
        require((path / "db/mdbx.dat").is_file(), f"not an existing Reth datadir: {path}")
        require((path / "db/mdbx.dat").stat().st_nlink == 1,
                "hard-linked databases are not independent writable copies")
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


def validate_schedule(config, schedule, head_timestamp):
    require(COBALT_ID < len(schedule) <= len(UPGRADES), "unsupported ProtocolVersions layout")
    for index, upgrade in enumerate(UPGRADES):
        activation = (config.get("base", {}).get(upgrade) if index >= 10
                      else config.get(upgrade + "_time"))
        expected = config["genesis"]["l2_time"] if activation == 0 else activation
        actual = schedule[index] if index < len(schedule) else 0
        if (expected is not None and expected <= head_timestamp) or (0 < actual <= head_timestamp):
            require(actual == expected,
                    f"contract would change historical {upgrade} activation")


class SnapshotFork:
    def __init__(self, directory, timeout=DEFAULT_TIMEOUT):
        self.directory = Path(directory).expanduser().resolve()
        self.timeout = timeout
        path = self.directory / "manifest.json"
        self.manifest = json.loads(path.read_text()) if path.exists() else None
        path = self.directory / "upstreams.json"
        self.credentials = json.loads(path.read_text()) if path.exists() else {}
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

    def status(self):
        endpoints = {}
        for role in ("l1", *ROLES, *(role + "-cl" for role in ROLES)):
            with contextlib.suppress(Unavailable):
                endpoints[role] = self.url(role)
        # Docker's raw ps/inspect output includes command arguments containing provider credentials.
        services = [{"service": item["Config"]["Labels"]["com.docker.compose.service"],
                     "running": item["State"]["Running"], "exit_code": item["State"].get("ExitCode", 0)}
                    for item in self.containers()]
        phase = self.manifest["phase"]
        # Container state only; status never probes RPCs to judge health.
        if phase == "running" and FORK_SERVICES - {item["service"] for item in services if item["running"]}:
            phase = "degraded"
        return {"project": self.manifest["project"], "phase": phase, "denim": self.denim_status(),
                "rpc_docker_host_only": endpoints, "services": services}

    def denim_status(self):
        """The journaled schedule, not proof that either L2 node has activated it."""
        timestamp = self.manifest.get("denim_timestamp")
        pending = self.manifest.get("pending_denim_timestamp")
        if timestamp is None:
            return ({"state": "unscheduled"} if pending is None
                    else {"state": "submission pending", "pending_timestamp": pending})
        remaining = timestamp - int(time.time())
        return {"state": "scheduled" if remaining > 0 else "activation time reached", "timestamp": timestamp,
                "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(timestamp)),
                "seconds_until_activation": max(0, remaining)}

    def endpoint(self, name):
        variable = self.manifest["upstreams"][name]
        value = os.environ.get(variable) or self.credentials.get(name, "")
        require(value.startswith(("http://", "https://")), f"set {variable} to an HTTP(S) endpoint")
        return value.rstrip("/")

    def compose_env(self):
        manifest = self.manifest
        keys = json.loads((self.directory / "keys.json").read_text())
        values = {
            "DIR": self.directory, "UID": os.getuid(), "GID": os.getgid(),
            "BASE_IMAGE": manifest["images"]["base"], "ANVIL_IMAGE": manifest["images"]["anvil"],
            "BATCHER_IMAGE": manifest["images"]["batcher"],
            "FORK_BLOCK": number(manifest["fork"]["number"]) if "fork" in manifest else UNKNOWN,
            "EPOCH_SLOTS": manifest["epoch_slots"], "SLOT_SECONDS": manifest.get("slot_seconds", UNKNOWN),
            "PROTOCOL_VERSIONS": manifest["protocol_versions"], "L1_PORT": manifest["port"],
            "SIGNER_KEY": keys["signer"], "BATCHER_KEY": keys["batcher"],
            # Placeholders allow stop/status without provider credentials. Start validates them.
            "L1_RPC": os.environ.get(manifest["upstreams"]["execution"]) or self.credentials.get("execution", "http://unconfigured.invalid"),
            "BEACON": os.environ.get(manifest["upstreams"]["beacon"]) or self.credentials.get("beacon", "http://unconfigured.invalid"),
        }
        for role in ROLES:
            values[role.upper() + "_DATADIR"] = manifest["datadirs"][role]
        return {**os.environ, **{"SNAPSHOT_" + key: str(value) for key, value in values.items()}}

    def compose(self, *args):
        require("fork" in self.manifest or "up" not in args or not FORK_SERVICES & set(args),
                "refusing to start fork services before F is discovered")
        # Announce lifecycle actions only; Compose output and its environment may carry credentials.
        for action, verb in (("up", "Starting"), ("stop", "Stopping")):
            if action in args:
                services = [arg for arg in args[args.index(action) + 1:] if not arg.startswith("-")]
                print(f"{verb} containers: {', '.join(services)}", file=sys.stderr, flush=True)
        self._containers = None
        return run("docker", "compose", "--project-name", self.manifest["project"],
                   "--file", str(COMPOSE), *args, env=self.compose_env(), timeout=self.timeout)

    def running(self):
        return bool(self.compose("--profile", "inspect", "ps", "--status", "running", "--quiet"))

    def await_rpc(self, role):
        def ready():
            try:
                return rpc(self.url(role), "eth_chainId")
            except Unavailable:
                self._containers = None
                return False
        wait(f"{role} execution RPC", ready, self.timeout)

    def inspect(self, discover=False):
        """Inspects both datadirs; with `discover`, the sequencer inspection also finds F."""
        inspector = os.environ.get("BASE_SNAPSHOT_INSPECTOR", str(ROOT / "target/debug/base-devnet"))
        arguments = []
        if "initial" in self.manifest:
            config = json.loads((self.directory / "config/rollup.json").read_text())
            require(config == self.manifest["initial"][0]["rollup_config"], "pinned rollup config changed")
            schedule = list(self.manifest["schedule"])
            if "denim_timestamp" in self.manifest:
                schedule = schedule[:DENIM_ID] + [self.manifest["denim_timestamp"]]
            for index, upgrade in enumerate(UPGRADES):
                timestamp = schedule[index] if index < len(schedule) else 0
                if index >= 10:
                    config.setdefault("base", {})[upgrade] = timestamp or None
                else:
                    config[upgrade + "_time"] = timestamp or None
            write_json(self.directory / "config/inspection.json", config)
            arguments = ["--rollup-config", str(self.directory / "config/inspection.json")]
        result = []
        require(not self.running_services() & set(ROLES),
                "production nodes are running on these datadirs; stop them before inspection")
        try:
            self.compose("--profile", "inspect", "up", "-d", "--no-build", "inspect-sequencer", "inspect-validator")
            for role in ROLES:
                self.await_rpc(role)
                command, env, timeout, secrets = [inspector, "inspect-snapshot", "--rpc-url", self.url(role),
                                                  *arguments], None, self.timeout, ()
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
                for label in ("safe_l2", "finalized_l2"):
                    saved = self.manifest.get("last_stop", {}).get(role, {}).get(label)
                    if saved:
                        block = rpc(self.url(role), "eth_getBlockByNumber", hex(saved["number"]), False)
                        # Reth may persist a head older than the recorded checkpoints; start() requires
                        # derivation to restore that missing tail before mining or sequencing.
                        unpersisted = block is None and saved["number"] > inspection["latest"]["block_info"]["number"]
                        require(unpersisted or (block and block["hash"] == saved["hash"]),
                                "restored L2 lost a recorded checkpoint")
        finally:
            self.compose("--profile", "inspect", "stop", "inspect-sequencer", "inspect-validator")
        return result

    def initialize(self, config, allow_write):
        require(allow_write, "init requires --allow-write for the two disposable working datadirs")
        for field, reason in OBSOLETE_CONFIG.items():
            require(field not in config, f"{field} is obsolete: {reason}")
        allowed = {"sequencer_datadir", "validator_datadir", "base_image", "anvil_image", "batcher_image",
                   "port", "epoch_slots", "protocol_versions", "execution_env", "beacon_env"}
        require(not config.keys() - allowed, "unknown configuration fields")
        datadirs = validate_paths(self.directory, [config["sequencer_datadir"], config["validator_datadir"]])
        port = number(config.get("port", 19545))
        require(1024 <= port <= 65535, "L1 port must be unprivileged")
        images = {role: run("docker", "image", "inspect", "--format", "{{.Id}}", config[role + "_image"])
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
                return
        else:
            self.manifest = {"version": 2, "project": "snapshot-" + secrets.token_hex(6), "phase": "inspecting",
                             **settings, "operations": {}}
        self.manifest["setup_input"] = config
        self.save()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", port))
        execution, beacon = self.endpoint("execution"), self.endpoint("beacon")
        require(number(rpc(execution, "eth_chainId", upstream=True)) == 1, "expected Ethereum mainnet upstream")
        genesis = request_json(beacon + "/eth/v1/beacon/genesis")["data"]
        duration = number(request_json(beacon + "/eth/v1/config/spec")["data"]["SECONDS_PER_SLOT"])
        require(self.manifest.get("beacon_genesis", genesis) == genesis
                and self.manifest.get("slot_seconds", duration) == duration, "upstream Beacon identity changed")
        self.manifest.update(beacon_genesis=genesis, slot_seconds=duration)
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
        # F is the L1 block whose canonical batches complete the snapshot's (safe, latest] tail.
        inspections = self.inspect(discover=True)
        discovered = inspections[0].pop("fork")
        header = rpc(execution, "eth_getBlockByNumber", hex(discovered["number"]), False, upstream=True)
        finalized = rpc(execution, "eth_getBlockByNumber", "finalized", False, upstream=True)
        validate_fork(discovered, header, finalized, number(genesis["genesis_time"]), duration)
        fork = {key: header[key] for key in ("number", "hash", "timestamp", "parentHash")}
        validate_boundary(inspections, discovered["number"])
        config = inspections[0]["rollup_config"]
        for inspection in inspections:
            for label in ("latest", "safe", "finalized"):
                origin = inspection[label]["block_info"]["l1origin"]
                origin_header = rpc(execution, "eth_getBlockByNumber", hex(origin["number"]), False, upstream=True)
                require(origin_header and origin_header["hash"] == origin["hash"], "snapshot has a noncanonical L1 origin")
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
        print(f"Prepared with L1 fork block {number(fork['number'])}; no local contracts changed. "
              "Start derives past F before sequencing.")

    def assert_local_l1(self):
        metadata = rpc(self.url("l1"), "anvil_metadata")
        fork = metadata.get("forkedNetwork") or {}
        require(number(metadata["chainId"]) == 1 and fork.get("forkBlockHash") == self.manifest["fork"]["hash"]
                and number(fork["forkBlockNumber"]) == number(self.manifest["fork"]["number"]),
                "local Anvil fork identity does not match manifest")
        require(request_json(self.url("l1") + "/eth/v1/beacon/genesis")["data"] == self.manifest["beacon_genesis"],
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
        require(number(receipt["status"]) == 1, f"{name} reverted; inspect its persisted receipt")
        operations[name]["receipt"] = receipt
        self.save()
        return receipt

    def bootstrap(self):
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
        self.save()

    def validate_restored_contracts(self):
        url = self.url("l1")
        actual = list(map(number, call(url, self.manifest["protocol_versions"], "getSchedule()(uint64[])")))
        expected = list(self.manifest["schedule"])
        pending = self.manifest.get("pending_denim_timestamp")
        if pending is not None and actual == expected[:DENIM_ID] + [pending]:
            self.record_denim(pending)
        if "denim_timestamp" in self.manifest:
            expected = expected[:DENIM_ID] + [self.manifest["denim_timestamp"]]
        require(actual == expected, "restored upgrade schedule differs from manifest")
        for address, implementation in self.manifest["contracts"].items():
            require(rpc(url, "eth_getStorageAt", address, IMPLEMENTATION_SLOT, "latest") == implementation,
                    "restored contract implementation differs from manifest")
        if not self.manifest.get("bootstrapped"):
            return
        system = self.manifest["system_config"]
        require(number(call(url, system, "batcherHash()(bytes32)")) == number(self.manifest["accounts"]["batcher"]),
                "restored batcher differs from manifest")
        require(call(url, system, "unsafeBlockSigner()(address)").lower() == self.manifest["accounts"]["signer"].lower(),
                "restored signer differs from manifest")

    def sync_status(self, role):
        return rpc(self.url(role + "-cl"), "optimism_syncStatus")

    def wait_upgrades(self):
        expected = list(self.manifest["schedule"])
        if "denim_timestamp" in self.manifest:
            expected = expected[:DENIM_ID] + [self.manifest["denim_timestamp"]]
        for role in ("sequencer", "validator"):
            def observed():
                config = rpc(self.url(role + "-cl"), "optimism_rollupConfig")
                ready = rpc(self.url(role + "-cl"), "base_upgradeReadiness")
                return ready["ready"] and all(
                    (config.get("base", {}).get(upgrade) if index >= 10 else config.get(upgrade + "_time"))
                    == (expected[index] or None)
                    for index, upgrade in enumerate(UPGRADES[:len(expected)])
                )
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
            return len(statuses) == len(ROLES) and derived_boundary(statuses, fork_number, initial)

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

    def wait_checkpoints(self):
        """Gates local L1 writes and sequencing on each node re-deriving its recorded safe and finalized
        blocks from retained L1. EL availability or unsafe height alone does not restore them."""
        last_stop = self.manifest.get("last_stop", {})
        checkpoints = [(role, last_stop[role][label]) for role in ROLES for label in ("safe_l2", "finalized_l2")
                       if last_stop.get(role, {}).get(label)]
        targets = {}
        for role, saved in checkpoints:
            targets[role] = max(targets.get(role, 0), number(saved["number"]))
        statuses = {}

        def recovered():
            try:
                statuses.update((role, self.sync_status(role)) for role in targets)
                if any(number(statuses[role]["safe_l2"]["number"]) < target for role, target in targets.items()):
                    return False
                blocks = [(role, saved, rpc(self.url(role), "eth_getBlockByNumber", hex(number(saved["number"])), False))
                          for role, saved in checkpoints]
            except Unavailable:
                self._containers = None
                return False
            for role, saved, block in blocks:
                require(block and block["hash"] == saved["hash"],
                        f"{role} derived a block conflicting with a recorded checkpoint; data preserved")
            return True

        def progress():
            heads = {role: number(status["safe_l2"]["number"]) for role, status in statuses.items()}
            return heads, "; ".join(f"{role} safe {safe}/{targets[role]} needed"
                                    for role, safe in heads.items()) or "consensus status unavailable"

        wait("nodes re-deriving recorded safe/finalized checkpoints", recovered, self.timeout, progress=progress)

    def peers(self, connect=True):
        infos = {role: rpc(self.url(role + "-cl"), "opp2p_self") for role in ROLES}
        for role, other in (("sequencer", "validator"), ("validator", "sequencer")):
            if connect:
                # Gossip stays on the internal network; resolve the peer's address there explicitly.
                _, ip = private_address(self.manifest["project"], (other,), self.containers())
                address = f"/ip4/{ip}/tcp/{GOSSIP_PORT}/p2p/{infos[other]['peerID']}"
                rpc(self.url(role + "-cl"), "opp2p_connectPeer", address)
            else:
                rpc(self.url(role + "-cl"), "opp2p_disconnectPeer", infos[other]["peerID"])

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

    def start(self):
        require(self.manifest["phase"] in ("prepared", "stopped", "running", "starting"), "init did not complete")
        print("Starting snapshot devnet: checking containers.", file=sys.stderr, flush=True)
        if self.running():
            if self.manifest["phase"] == "running" and FORK_SERVICES <= self.running_services():
                self.assert_local_l1()
                self.validate_restored_contracts()
                if all(self.consensus_ready(role) for role in ROLES) and rpc(
                        self.url("sequencer-cl"), "admin_sequencerActive"):
                    self.schedule_denim()
                    print("Snapshot devnet already running.", flush=True)
                    return
            self.stop()
        validate_paths(self.directory, self.manifest["datadirs"].values())
        self.endpoint("execution")
        self.endpoint("beacon")
        previous_phase = self.manifest["phase"]
        # Never reuse a stopped fork whose durable L1 state vanished.
        if previous_phase != "prepared":
            require((self.directory / "l1/anvil.json").is_file(), "L1 state is missing; refusing a fresh fork")
        self.manifest["phase"] = "starting"
        self.save()
        try:
            self.compose("up", "-d", "--no-build", "l1")
            self.await_rpc("l1")
            self.assert_local_l1()
            self.validate_restored_contracts()
            inspections = self.inspect()
            for snapshot in inspections:
                for label in ("latest", "safe", "finalized"):
                    origin = snapshot[label]["block_info"]["l1origin"]
                    header = rpc(self.url("l1"), "eth_getBlockByNumber", hex(origin["number"]), False)
                    require(header and header["hash"] == origin["hash"], "restored L2 refers to missing/conflicting L1 history")
            require(not self.running_services() & {"inspect-" + role for role in ROLES},
                    "inspection nodes still hold the datadirs")
            self.compose("up", "-d", "--no-build", "sequencer", "validator")
            for role in ROLES:
                self.await_rpc(role)
                wait(role + " consensus RPC", lambda: self.consensus_ready(role), self.timeout)
            require(not rpc(self.url("validator-cl"), "admin_sequencerActive"),
                    "validator must remain stopped and derive independently")
            self.wait_checkpoints()
            if not self.manifest.get("boundary_validated"):
                self.wait_boundary()
            if not self.manifest.get("bootstrapped"):
                self.bootstrap()
                # The startup signer read must use the new local SystemConfig.
                self.compose("stop", "sequencer", "validator")
                self.compose("up", "-d", "--no-build", "sequencer", "validator")
                for role in ROLES:
                    self.await_rpc(role)
                    wait(role + " consensus RPC", lambda: self.consensus_ready(role), self.timeout)
                self.manifest["bootstrapped"] = True
                self.save()
            self.wait_upgrades()
            self.mine()
            rpc(self.url("l1"), "anvil_setIntervalMining", self.manifest["slot_seconds"])
            self.peers()
            try:
                # op-batcher exits, even with code 0, when it cannot call miner_setMaxDASize.
                rpc(self.url("sequencer"), "miner_getMaxDASize")
            except Unavailable as error:
                raise RuntimeError("sequencer HTTP RPC lacks the miner API required by the batcher; "
                                   "data preserved, recreate services from the current Compose file") from error
            head = rpc(self.url("sequencer"), "eth_getBlockByNumber", "latest", False)
            rpc(self.url("sequencer-cl"), "admin_startSequencer", head["hash"])
            # Derivation authorizes batch senders by the SystemConfig at each batch's L1 inclusion
            # block, not the L2 block's origin, so old-origin catch-up blocks are batchable now.
            # Deferring batching until wall time would risk their sequencing windows expiring.
            self.start_batcher()
            unsafe = {}

            def caught_up():
                self.require_batcher()
                try:
                    unsafe.update(self.sync_status("sequencer")["unsafe_l2"])
                except Unavailable:
                    return False
                return unsafe["timestamp"] >= int(time.time()) - 2 * self.manifest["slot_seconds"]

            def progress():
                if not unsafe:
                    return None, "sequencer status unavailable"
                return unsafe["number"], (
                    f"unsafe L2 {unsafe['number']} at {unsafe['timestamp']} "
                    f"({max(0, int(time.time()) - unsafe['timestamp'])}s behind wall time), "
                    f"L1 origin {unsafe['l1origin']['number']}")
            wait("sequencer catch-up to wall time with a live batcher", caught_up, self.timeout, progress=progress)
            self.manifest["phase"] = "running"
            self.save()
            print(f"Sequencer RPC: {self.url('sequencer')}\nValidator RPC: {self.url('validator')}\n"
                  "These internal-network addresses work only on this Docker host and change when "
                  "containers are recreated; rerun status for current values.")
        except (Exception, KeyboardInterrupt):
            # Keep L1 alive while stopping dependents; never remove state on failure.
            self.compose("stop", "batcher", "sequencer", "validator")
            self.compose("stop", "l1")
            raise
        # start_batcher already proved both nodes independently derive and agree on a newly batched
        # block. A scheduling failure leaves the healthy fork and its durable journal in place.
        try:
            self.schedule_denim()
        except RuntimeError as error:
            raise RuntimeError(f"fork is running but Denim scheduling did not complete ({error}); "
                               "rerun schedule-denim to resume") from error

    def consensus_ready(self, role):
        """False while the role is stopped or starting; identity/safety failures still raise."""
        try:
            return bool(self.sync_status(role))
        except Unavailable:
            return False

    def stop(self):
        failure = None
        if self.running():
            try:
                if self.consensus_ready("sequencer") and rpc(self.url("sequencer-cl"), "admin_sequencerActive"):
                    rpc(self.url("sequencer-cl"), "admin_stopSequencer")
            except Unavailable:
                pass  # Stopping its container below also stops sequencing.
            except RuntimeError as error:
                failure = error  # Still stop every container below, then report it.
            self.compose("stop", "batcher")
            # Persist diagnostics; successful shutdown is not an atomic cross-chain checkpoint.
            statuses = {}
            for role in ROLES:
                try:
                    statuses[role] = self.sync_status(role)
                except Unavailable:
                    pass  # A stopped role has nothing to record.
                except RuntimeError as error:
                    failure = failure or error
            self.manifest["last_stop"] = statuses
            self.save()
            self.compose("stop", "sequencer", "validator", "inspect-sequencer", "inspect-validator")
            self.compose("stop", "l1")
        if self.manifest["phase"] not in ("prepared", "inspecting"):
            self.manifest["phase"] = "stopped"
        self.save()
        if failure:
            raise failure

    def record_denim(self, timestamp):
        """Journals Denim at `timestamp`, already on local L1; a pending submission must have succeeded."""
        if self.manifest.get("pending_denim_timestamp") is not None:
            operation = self.manifest["operations"].get("schedule-denim", {})
            require("hash" in operation, "Denim submission was interrupted; reconcile its nonce manually")
            receipt = rpc(self.url("l1"), "eth_getTransactionReceipt", operation["hash"])
            require(receipt and number(receipt["status"]) == 1, "Denim transaction missing from restored L1")
            operation["receipt"] = receipt
        self.manifest["denim_timestamp"] = timestamp
        self.manifest.pop("pending_denim_timestamp", None)
        self.save()

    def schedule_denim(self, timestamp=None):
        """Schedules Denim at `timestamp`, by default the earliest even timestamp the live contract
        notice allows. An existing schedule is preserved and a journaled submission is reconciled,
        never moved or resent. Returns once both nodes observe the recorded schedule."""
        self.assert_local_l1()
        require(self.manifest["phase"] == "running", "start the fork before scheduling Denim")
        url, contract = self.url("l1"), self.manifest["protocol_versions"]
        schedule = list(map(number, call(url, contract, "getSchedule()(uint64[])")))
        require(schedule[:DENIM_ID] == self.manifest["schedule"][:DENIM_ID], "historical schedule changed")
        require(len(schedule) in (DENIM_ID, DENIM_ID + 1), "unsupported schedule; no implicit registrations")
        current = schedule[DENIM_ID] if len(schedule) > DENIM_ID else 0
        if "pending_denim_timestamp" in self.manifest and "schedule-denim" not in self.manifest["operations"]:
            # send() journals the transaction before submitting it, so none was ever sent.
            del self.manifest["pending_denim_timestamp"]
            self.save()
        pending = self.manifest.get("pending_denim_timestamp")
        # A timestamp set outside this launcher is preserved too.
        existing = pending if pending is not None else self.manifest.get("denim_timestamp", current or None)
        if existing is not None:
            state = "scheduled" if pending is None else "pending"
            require(timestamp in (None, existing), f"Denim is already {state} at {existing}; refusing to move it")
            timestamp = existing
            if current == timestamp:
                self.record_denim(timestamp)
                self.wait_upgrades()
                self.report_denim(timestamp, "already scheduled")
                return
            require(pending is not None, "Denim on local L1 differs from manifest")
        else:
            l1_time = number(rpc(url, "eth_getBlockByNumber", "latest", False)["timestamp"])
            l2_time = number(rpc(self.url("sequencer"), "eth_getBlockByNumber", "latest", False)["timestamp"])
            notice = number(call(url, contract, "MIN_NOTICE()(uint64)"))
            slot = self.manifest["slot_seconds"]
            # The contract measures notice from the inclusion block, mined in a later interval slot.
            earliest = max(l1_time, l2_time, int(time.time())) + notice + slot
            if timestamp is None:
                # One more slot absorbs clock truncation and submission latency; Denim cannot precede Cobalt.
                timestamp = max(earliest + slot, schedule[COBALT_ID])
                timestamp += timestamp % 2
            require(timestamp >= earliest,
                    f"Denim requires the contract's {notice}s notice plus a mining-slot margin (earliest {earliest})")
            require(schedule[COBALT_ID] != 0 and schedule[COBALT_ID] <= timestamp,
                    "Cobalt must already be scheduled; do not fabricate historical activations")
            require(timestamp % 2 == 0, "choose a Denim timestamp on the pre-Denim two-second grid")
            self.manifest["pending_denim_timestamp"] = timestamp
            self.save()
            print(f"Scheduling Denim at L2 timestamp {timestamp} "
                  f"({time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(timestamp))}) with the contract's "
                  f"{notice}s notice", flush=True)
        minimum = call(url, contract, "minimumProtocolVersion()(uint256)")
        owner = call(url, contract, "proxyAdminOwner()(address)")
        if len(schedule) == DENIM_ID:
            # The real contract interprets zero as retaining the current minimum version.
            self.send("schedule-denim", owner, contract, "registerUpgrade(uint64,uint256)", timestamp, 0)
        else:
            self.send("schedule-denim", owner, contract, "setTimestamp(uint256,uint64)", DENIM_ID, timestamp)
        actual = list(map(number, call(url, contract, "getSchedule()(uint64[])")))
        require(actual[:DENIM_ID] == schedule[:DENIM_ID] and actual[DENIM_ID:] == [timestamp],
                "unexpected schedule after Denim transaction")
        require(call(url, contract, "minimumProtocolVersion()(uint256)") == minimum,
                "Denim transaction changed the minimum protocol version")
        self.record_denim(timestamp)
        self.wait_upgrades()
        self.report_denim(timestamp, "scheduled")

    def report_denim(self, timestamp, outcome):
        remaining = timestamp - int(time.time())
        when = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(timestamp))
        print(f"Denim {outcome} at L2 timestamp {timestamp} ({when}); both nodes observe it. "
              + (f"Activation in {remaining}s (~{remaining // 60}m{remaining % 60:02d}s) by wall clock."
                 if remaining > 0 else f"Activation time passed {-remaining}s ago by wall clock."), flush=True)

    def deposit(self, amount):
        require(amount > 0, "deposit amount must be positive")
        user = self.manifest["accounts"]["user"]
        return self.send("fund-user", user, self.manifest["portal"],
                         "depositTransaction(address,uint256,uint64,bool,bytes)", user, amount, 100000, "false", "0x", value=amount)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--dir", help="override the fork directory selected by setup")
    common.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT,
                        help="seconds per readiness gate (default: 1800); derivation and catch-up gates fail only after "
                             "this long without head progress")
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("setup", parents=[common], help="prepare or resume an experiment; completed steps are reused")
    prepare.add_argument("--workdir", help="new or interrupted working directory (remembered on retry)")
    prepare.add_argument("--download-concurrency", type=int, default=DEFAULT_DOWNLOAD_CONCURRENCY,
                         help="maximum simultaneous HTTP downloads, including file chunks (default: %(default)s)")
    prepare.add_argument("--anvil-image", default="base-anvil:snapshot-24ec5e47")
    prepare.add_argument("--batcher-image", default="op-batcher:local")
    init = commands.add_parser("init", parents=[common])
    init.add_argument("--config", required=True)
    init.add_argument("--allow-write", action="store_true")
    for command, aliases in (("start", ["up"]), ("stop", ["down"]), ("status", [])):
        commands.add_parser(command, aliases=aliases, parents=[common]).set_defaults(command=command)
    reset = commands.add_parser("reset", parents=[common], help="retire the fork directory without deleting any datadir")
    reset.add_argument("--confirm-project", required=True)
    schedule = commands.add_parser("schedule-denim", parents=[common])
    schedule.add_argument("timestamp", type=int, nargs="?",
                          help="even L2 timestamp; defaults to the earliest the contract notice allows")
    deposit = commands.add_parser("deposit", parents=[common])
    deposit.add_argument("--wei", type=int, default=10**18)
    args = parser.parse_args()
    require(args.timeout > 0, "timeout must be positive")
    if args.command == "setup":
        setup(args)
        return
    require(args.command != "init" or args.dir, "init requires --dir; use setup for the guided workflow")
    args.dir = configured_directory(args.dir)
    fork = SnapshotFork(args.dir, args.timeout)
    if args.command == "init":
        require(not fork.directory.exists() or fork.manifest is not None
                or {entry.name for entry in fork.directory.iterdir()} <= {".lock", "upstreams.json", "manifest.json.tmp"},
                "init directory contains unrecognized data; existing data is never overwritten")
        fork.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    require(fork.directory.is_dir(), "fork directory does not exist")
    with open(fork.directory / ".lock", "a") as lock:
        # Manifests are atomically replaced, so read-only status is safe during a long start.
        if args.command != "status":
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fork = SnapshotFork(args.dir, args.timeout)
        if args.command == "init":
            fork.initialize(json.loads(Path(args.config).read_text()), args.allow_write)
        else:
            require(fork.manifest is not None and fork.manifest["version"] == 2,
                    "unsupported/missing manifest; manifests from launchers requiring fork_block cannot resume")
            if args.command == "status":
                print(json.dumps(fork.status(), indent=2))
            elif args.command == "reset":
                require(args.confirm_project == fork.manifest["project"], "project confirmation does not match")
                require(not fork.running(), "stop the fork before retiring it")
                destination = fork.directory.with_name(fork.directory.name + ".retired-" + secrets.token_hex(4))
                fork.directory.rename(destination)
                print(f"Preserved fork state at {destination}; restore fresh working datadirs before init.")
            elif args.command == "schedule-denim":
                fork.schedule_denim(args.timestamp)
            elif args.command == "deposit":
                print(json.dumps(fork.deposit(args.wei), indent=2))
            else:
                getattr(fork, args.command)()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("snapshot devnet: interrupted; data preserved, rerun the same command to continue", file=sys.stderr)
        sys.exit(130)
    except (RuntimeError, OSError, ValueError, KeyError, EOFError) as error:
        print(f"snapshot devnet: {error}", file=sys.stderr)
        sys.exit(1)
