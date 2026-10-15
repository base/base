#!/usr/bin/env python3
import hashlib
import json
import os
from pathlib import Path
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
