#!/usr/bin/env python3
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

RECIPES = Path(__file__).resolve().parent


def run(*args, **kwargs):
    return subprocess.run(args, text=True, capture_output=True, **kwargs)


class CheckoutTests(unittest.TestCase):
    def test_immutable_checkout_and_fetch_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            (repo / "input").write_text("source\n")
            subprocess.run(["git", "-C", str(repo), "add", "input"], check=True)
            subprocess.run(["git", "-C", str(repo), "-c", "user.name=Test", "-c",
                            "user.email=test@example.com", "-c", "commit.gpgsign=false",
                            "commit", "-qm", "fixture"], check=True)
            sha = run("git", "-C", str(repo), "rev-parse", "HEAD").stdout.strip()
            script = str(RECIPES / "checkout.sh")
            result = run("sh", script, str(root / "valid"), str(repo), sha)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((root / "valid" / "input").read_text(), "source\n")
            for revision in ("HEAD", sha[:12], "g" * 40, "0" * 40):
                destination = root / revision
                result = run("sh", script, str(destination), str(repo), revision)
                self.assertNotEqual(result.returncode, 0, revision)
                self.assertFalse((destination / "input").exists())
            result = run("sh", script, str(root / "missing"), str(root / "absent"), sha)
            self.assertNotEqual(result.returncode, 0)

    def test_tag_object_does_not_pass_commit_verification(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root / "repo")], check=True)
            git = ["git", "-C", str(root / "repo"), "-c", "user.name=Test", "-c",
                   "user.email=test@example.com", "-c", "commit.gpgsign=false",
                   "-c", "tag.gpgsign=false"]
            subprocess.run(git + ["commit", "--allow-empty", "-qm", "fixture"], check=True)
            subprocess.run(git + ["tag", "-am", "tag", "tag"], check=True)
            sha = run(*git, "rev-parse", "tag").stdout.strip()
            result = run("sh", str(RECIPES / "checkout.sh"), str(root / "copy"),
                         str(root / "repo"), sha)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("fetched commit differs", result.stderr)


class InstallTests(unittest.TestCase):
    def test_linuxkit_requires_successful_download_and_checksum(self):
        payload = b"linuxkit fixture\n"
        checksum = hashlib.sha256(payload).hexdigest()
        for failure, download_exit, digest, expected_exit in (
            ("download", 37, checksum, 37),
            ("checksum", 0, "0" * 64, 1),
            ("valid", 0, checksum, 0),
        ):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                script = root / "install.sh"
                # Redirect only the install destination; keep the real checksum and chmod.
                script.write_text((RECIPES / "install.sh").read_text().replace(
                    "/usr/local/bin/linuxkit", "./linuxkit"))
                wget = root / "wget"
                wget.write_text("#!/bin/sh\nprintf 'linuxkit fixture\\n' > linuxkit\n"
                                'exit "$DOWNLOAD_EXIT"\n')
                wget.chmod(0o755)
                env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                           DOWNLOAD_EXIT=str(download_exit), LINUXKIT_SHA256=digest)
                result = run("/bin/sh", str(script), "linuxkit", cwd=root, env=env)
                self.assertEqual(result.returncode, expected_exit, result.stderr)
                self.assertEqual((root / "linuxkit").read_bytes(), payload)
                self.assertEqual(bool((root / "linuxkit").stat().st_mode & 0o111),
                                 failure == "valid")
                if failure == "checksum":
                    self.assertIn("FAILED", result.stdout)

    def test_nitro_installer_requires_verified_download_and_pinned_compiler(self):
        payload = (b'#!/bin/sh\ncase "${0##*/}" in rustup-init*) ;;\n'
                   b'*) echo "installer requires a rustup-init basename" >&2; exit 91 ;; esac\n'
                   b'printf "%s\\n" "$@" > "$INSTALLED"\n')
        checksum = hashlib.sha256(payload).hexdigest()
        for scenario, arch, download_exit, digest, compiler, expected_exit in (
            ("checksum", "x86_64", 0, "0" * 64, "1.99.0", 1),
            ("download", "x86_64", 37, checksum, "1.99.0", 37),
            ("valid", "x86_64", 0, checksum, "1.99.0", 0),
            ("valid", "aarch64", 0, checksum, "1.99.0", 0),
            ("valid", "arm64", 0, checksum, "1.99.0", 0),
            ("cargo-home", "x86_64", 0, checksum, "1.99.0", 0),
            ("unsupported", "riscv64", 0, checksum, "1.99.0", 1),
            ("compiler", "x86_64", 0, checksum, "1.98.0", 1),
        ):
            with self.subTest(scenario=scenario, arch=arch), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "payload").write_bytes(payload)
                cargo_bin = root / ("custom-cargo" if scenario == "cargo-home" else ".cargo") / "bin"
                cargo_bin.mkdir(parents=True)
                rustc = cargo_bin / "rustc"
                rustc.write_text('#!/bin/sh\nprintf "rustc %s (fixture)\\n" "$COMPILER"\n')
                rustc.chmod(0o755)
                for tool, command in {
                    "apt-get": "exit 0",
                    "uname": 'echo "$ARCH"',
                    "curl": 'printf "%s\\n" "$@" > "$HOME/download"\n'
                            'for arg do destination=$arg; done\n'
                            'cp "$HOME/payload" "$destination"\nexit "$DOWNLOAD_EXIT"',
                    "chmod": 'touch "$HOME/chmod-called"; /bin/chmod "$@"',
                    "rm": 'if [ "$1" = -rf ]; then exit 0; fi\n'
                          'for arg do installer=$arg; done\n'
                          'if [ -x "$installer" ]; then touch "$HOME/executable"; fi\n'
                          'touch "$HOME/cleaned"; /bin/rm "$@"',
                }.items():
                    path = root / tool
                    path.write_text("#!/bin/sh\n" + command + "\n")
                    path.chmod(0o755)
                env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", HOME=str(root),
                           ARCH=arch, DOWNLOAD_EXIT=str(download_exit), COMPILER=compiler,
                           TMPDIR=str(root), INSTALLED=str(root / "installed"),
                           RUSTUP_SHA256_AMD64=digest, RUSTUP_SHA256_ARM64=digest)
                for variable in ("RUST_TOOLCHAIN", "RUSTUP_VERSION", "CARGO_HOME"):
                    env.pop(variable, None)
                if scenario == "cargo-home":
                    env["CARGO_HOME"] = str(cargo_bin.parent)
                result = run("/bin/sh", str(RECIPES / "install.sh"), "nitro-cli", env=env)
                self.assertEqual(result.returncode, expected_exit, result.stderr)
                if (root / "download").exists():
                    installer = Path((root / "download").read_text().splitlines()[-1])
                    self.assertFalse(installer.exists(), "temporary installer was not removed")
                if scenario in ("download", "checksum", "unsupported"):
                    self.assertFalse((root / "installed").exists(), "unverified installer ran")
                    self.assertFalse((root / "chmod-called").exists(), "unverified installer reached chmod")
                    self.assertFalse((root / "executable").exists(), "unverified installer was executable")
                    if scenario == "download":
                        self.assertEqual(result.stdout, "", "failed download reached verification")
                    elif scenario == "checksum":
                        self.assertIn("FAILED", result.stdout)
                    else:
                        self.assertIn("unsupported architecture", result.stderr)
                        self.assertFalse((root / "download").exists())
                else:
                    self.assertTrue((root / "executable").exists())
                    self.assertTrue((root / "chmod-called").exists())
                    self.assertEqual((root / "installed").read_text().splitlines(),
                                     ["-y", "--default-toolchain", "1.99.0"])
                    target = "x86_64" if arch == "x86_64" else "aarch64"
                    self.assertIn(f"https://static.rust-lang.org/rustup/archive/1.29.1/"
                                  f"{target}-unknown-linux-gnu/rustup-init",
                                  (root / "download").read_text().splitlines())
                    self.assertIn(f"rustc {compiler}", result.stdout)
                if scenario != "unsupported":
                    self.assertTrue((root / "cleaned").exists())

    def test_bad_download_hash_stops_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            for tool, command in {
                "apt-get": "exit 0",
                "rm": "exit 0",
                "uname": "echo x86_64",
                "wget": 'touch "${2##*/}"',
                "tar": 'touch "$EXTRACTED"',
                "unzip": 'touch "$EXTRACTED"',
            }.items():
                path = tools / tool
                path.write_text("#!/bin/sh\n" + command + "\n")
                path.chmod(0o755)
            env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}",
                       EXTRACTED=str(root / "extracted"))
            result = run("sh", str(RECIPES / "install.sh"), "zk-host", cwd=root, env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("FAILED", result.stdout)
            self.assertFalse((root / "extracted").exists())



class LockfileTests(unittest.TestCase):
    def test_builds_reject_missing_and_stale_lockfiles(self):
        components = {
            "nitro-host": ("base-prover-nitro-host", "base-prover-nitro-host"),
            "proposer": ("base-proposer-bin", "base-proposer"),
            "challenger": ("base-challenger-bin", "base-challenger"),
            "registrar": ("base-proof-tee-registrar-bin", "base-proof-tee-registrar"),
            "prover-service": ("base-prover-service-bin", "base-prover-service"),
            "zk-host": ("base-prover-zk-host", "base-prover-zk-host"),
            "snark-e2e": ("base-snark-e2e-bin", "base-snark-e2e"),
            "challenger-e2e": ("base-challenger-e2e-bin", "base-challenger-e2e"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            fixture = root / "fixture"
            fixture.mkdir()
            packages = list(components.values()) + [("range", "range"),
                                                     ("aggregation", "aggregation")]
            (fixture / "Cargo.toml").write_text(
                '[workspace]\nresolver = "2"\nmembers = ["shared", '
                + ", ".join(json.dumps("packages/" + package) for package, _ in packages) + "]\n")
            shared = fixture / "shared"
            shared.mkdir()
            (shared / "Cargo.toml").write_text(
                '[package]\nname = "shared"\nversion = "0.1.0"\nedition = "2021"\n[lib]\npath = "lib.rs"\n')
            (shared / "lib.rs").write_text("pub fn value() {}\n")
            for package, binary in packages:
                member = fixture / "packages" / package
                member.mkdir(parents=True)
                (member / "Cargo.toml").write_text(
                    f'[package]\nname = "{package}"\nversion = "0.1.0"\nedition = "2021"\n'
                    f'[[bin]]\nname = "{binary}"\npath = "main.rs"\n'
                    '[dependencies]\nshared = { path = "../../shared" }\n')
                (member / "main.rs").write_text("fn main() { shared::value(); }\n")
            result = run("cargo", "generate-lockfile", "--offline", cwd=fixture)
            self.assertEqual(result.returncode, 0, result.stderr)
            lock = fixture / "Cargo.lock"
            (fixture / "valid.lock").write_bytes(lock.read_bytes())
            cargo = tools / "cargo"
            # Only the guest target and toolchain need translation for stock Cargo.
            cargo.write_text(f"#!{os.sys.executable}\n" + '''import json, os, pathlib, shutil, subprocess, sys
root = pathlib.Path(os.environ["FIXTURE"])
args = sys.argv[1:]
guest = "--target" in args
package = ("range" if pathlib.Path.cwd().name == "ethereum" else "aggregation") if guest else args[args.index("--package") + 1]
state = os.environ["LOCK_STATE"] if package == os.environ["SELECTED_PACKAGE"] else "valid"
lock = root / "Cargo.lock"
lock.write_bytes((root / "valid.lock").read_bytes())
manifest = root / "shared/Cargo.toml"
manifest.write_text(manifest.read_text().replace('0.2.0', '0.1.0'))
if state == "absent":
    lock.unlink()
elif state == "stale":
    manifest.write_text(manifest.read_text().replace('0.1.0', '0.2.0'))
before = lock.read_bytes() if lock.exists() else None
original = args.copy()
if guest:
    args[args.index("--target") + 1] = os.environ["HOST_TARGET"]
    args += ["--package", package]
env = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"), RUSTC=os.environ["REAL_RUSTC"])
for key in ("RUSTUP_TOOLCHAIN", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_BOOTSTRAP"):
    env.pop(key, None)
result = subprocess.run([os.environ["REAL_CARGO"], *args, "--offline"], cwd=root, env=env)
after = lock.read_bytes() if lock.exists() else None
with open(os.environ["BUILD_LOG"], "a") as log:
    log.write(json.dumps([package, original, result.returncode, before == after]) + "\\n")
if result.returncode == 0 and guest:
    target = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / "riscv64im-succinct-zkvm-elf/release"
    target.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(root / "target" / os.environ["HOST_TARGET"] / "release" / package, target / package)
sys.exit(result.returncode)
''')
            cargo.chmod(0o755)
            rustc = tools / "rustc"
            rustc.write_text('#!/bin/sh\nunset RUSTUP_TOOLCHAIN\nexec "$REAL_RUSTC" "$@"\n')
            rustc.chmod(0o755)
            version = run("rustc", "-vV").stdout
            host = next(line.split(": ", 1)[1] for line in version.splitlines()
                        if line.startswith("host: "))
            env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}",
                       FIXTURE=str(fixture), REAL_CARGO=shutil.which("cargo"),
                       REAL_RUSTC=shutil.which("rustc"), HOST_TARGET=host,
                       BUILD_LOG=str(root / "build.log"))
            for recipe in ("build.sh", "elf.sh"):
                (root / recipe).write_text((RECIPES / recipe).read_text()
                                          .replace("/app", str(fixture))
                                          .replace("/root/program", str(root)))
            for guest in ("range/ethereum", "aggregation"):
                (root / "crates/proof/zk/programs/succinct" / guest).mkdir(parents=True)
            cases = [(component, package, binary)
                     for component, (package, binary) in components.items()]
            cases += [("elf", guest, guest) for guest in ("range", "aggregation")]
            for component, package, binary in cases:
                for state in ("valid", "absent", "stale"):
                    with self.subTest(component=component, package=package, state=state):
                        Path(env["BUILD_LOG"]).write_text("")
                        env.update(SELECTED_PACKAGE=package, LOCK_STATE=state)
                        args = ["/bin/sh", str(root / ("elf.sh" if component == "elf" else "build.sh"))]
                        if component != "elf":
                            args += [component, "dev"]
                        result = run(*args, cwd=fixture, env=env)
                        calls = [json.loads(line) for line in Path(env["BUILD_LOG"]).read_text().splitlines()]
                        selected = next(call for call in calls if call[0] == package)
                        detail = f"argv={selected[1]}\n{result.stderr}"
                        if state == "valid":
                            self.assertEqual(result.returncode, 0, detail)
                            self.assertTrue(selected[3], "valid lockfile changed: " + detail)
                            if component != "elf":
                                self.assertTrue((fixture / binary).is_file())
                        else:
                            self.assertNotEqual(result.returncode, 0, detail)
                            self.assertIn("--locked", result.stderr)
                            self.assertTrue(selected[3], "rejected lockfile changed: " + detail)



class WrapperTests(unittest.TestCase):
    def test_host_build_scripts_only_and_argument_boundaries(self):
        compiler = "import json,sys; print(json.dumps(sys.argv[1:]))"
        for crate, suffix in (("build_script_build", ["-C", "target-feature=-crt-static"]),
                              ("build_script_custom", ["-C", "target-feature=-crt-static"]),
                              ("base_prover_nitro_enclave", [])):
            args = ["-c", compiler, "--crate-name", crate, "path with spaces.rs",
                    "-C", "target-feature=+crt-static"]
            result = run("sh", str(RECIPES / "rustc-wrapper.sh"), os.sys.executable, *args)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), args[2:] + suffix)


if __name__ == "__main__":
    unittest.main()
