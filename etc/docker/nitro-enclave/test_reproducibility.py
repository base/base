#!/usr/bin/env python3
import hashlib
import json
import os
from pathlib import Path
import shlex
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

sys.dont_write_bytecode = True
import reproduce

HERE = Path(__file__).resolve().parent


class ReproducibilityTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("rustc"), "rustc is required")
    def test_rust_source_paths_and_metadata_do_not_change_objects(self):
        objects = []
        with tempfile.TemporaryDirectory() as tmp:
            for name, timestamp, mode in [("first", 1, 0o644), ("second", 1700000000, 0o600)]:
                root = Path(tmp) / name
                root.mkdir()
                source = root / "fixture.rs"
                source.write_text('pub fn source_path() -> &\'static str { file!() }\n')
                source.chmod(mode)
                os.utime(source, (timestamp, timestamp))
                env = dict(os.environ, NITRO_SOURCE_ROOT=str(root))
                flags = subprocess.check_output(
                    ["sh", "-c", '. "$1"; printf "%s" "$CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS"', "sh", str(HERE / "build-env.sh")],
                    env=env, text=True,
                )
                output = root / "fixture.o"
                subprocess.run(
                    ["rustc", "--crate-name", "fixture", "--crate-type", "lib", "--emit=obj", "-o", str(output), str(source), *shlex.split(flags)],
                    check=True,
                )
                objects.append(hashlib.sha256(output.read_bytes()).hexdigest())
        self.assertEqual(objects[0], objects[1], "Rust source-root paths leaked into object bytes")

    def test_committed_context_ignores_checkout_metadata_and_untracked_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "checkout"
            root.mkdir()
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            source = root / "input.rs"
            source.write_text("const VALUE: u64 = 42;\n")
            subprocess.run(["git", "-C", str(root), "add", "."], check=True)
            tree = subprocess.check_output(["git", "-C", str(root), "write-tree"], text=True).strip()
            first, second = Path(tmp) / "first.tar", Path(tmp) / "second.tar"
            reproduce.context(tree, first, root)
            os.utime(source, (1700000000, 1700000000))
            source.chmod(0o777)
            source.write_text("local dirty content must not enter a committed build")
            (root / "untracked.rs").write_text("not part of the build")
            reproduce.context(tree, second, root)
            self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_filtered_context_retains_compile_time_inputs(self):
        sources = subprocess.check_output(["git", "ls-files", "*.rs"], cwd=reproduce.ROOT, text=True).splitlines()
        inputs = set()
        for name in sources:
            source = reproduce.ROOT / name
            for literal in re.findall(r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)', source.read_text()):
                included = (source.parent / literal).resolve()
                if included.is_file() and included.is_relative_to(reproduce.ROOT):
                    inputs.add(str(included.relative_to(reproduce.ROOT)))
        self.assertIn("bin/prover/nitro-enclave/README.md", inputs)
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "context.tar"
            reproduce.context("HEAD", archive)
            with tarfile.open(archive) as tar:
                missing = inputs - set(tar.getnames())
        self.assertEqual(missing, set(), f"Compile-time inputs missing from canonical context: {sorted(missing)}")

    def test_comparison_accepts_cloned_daemon_ids_and_rejects_tampering(self):
        with tempfile.TemporaryDirectory() as tmp:
            first, second = Path(tmp) / "first", Path(tmp) / "second"
            for directory in (first, second):
                for name in reproduce.ARTIFACTS:
                    path = directory / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(name.encode())
                reproduce.write_json(directory / "identity.json", {"source_identity": "source"})
                reproduce.write_json(directory / "measurements.json", {"PCR0": "ab" * 48})
                reproduce.write_json(directory / "host.json", {
                    "docker_id": "cloned-daemon-id", "name": directory.name,
                    "architecture": "amd64", "buildkit": reproduce.BUILDKIT,
                    "ci": {"job": directory.name, "attempt": "run/1/" + directory.name},
                })
            expected = reproduce.compare(first, second, require_independent=True)
            second_host = json.loads((second / "host.json").read_text())
            second_host["name"] = first.name
            reproduce.write_json(second / "host.json", second_host)
            with self.assertRaisesRegex(ValueError, "Independent.*hosts"):
                reproduce.compare(first, second, require_independent=True)
            second_host["name"] = second.name
            second_host["ci"] = {"job": first.name, "attempt": "run/1/" + first.name}
            reproduce.write_json(second / "host.json", second_host)
            with self.assertRaisesRegex(ValueError, "CI job and attempt"):
                reproduce.compare(first, second, require_independent=True)
            second_host["ci"] = {"job": second.name, "attempt": "run/1/" + second.name}
            reproduce.write_json(second / "host.json", second_host)
            baseline = Path(tmp) / "baseline.json"
            reproduce.write_json(baseline, {"measurements": expected})
            (second / "eif.bin").write_bytes(b"different EIF")
            with self.assertRaisesRegex(ValueError, "Full EIF bytes differ"):
                reproduce.compare(first, second)
            shutil.copyfile(first / "eif.bin", second / "eif.bin")
            reproduce.write_json(second / "measurements.json", {"PCR0": "cd" * 48})
            with self.assertRaisesRegex(ValueError, "pcr0:"):
                reproduce.compare(first, second)
            shutil.copyfile(first / "measurements.json", second / "measurements.json")
            reproduce.write_json(first / "identity.json", {"source_identity": "new source"})
            reproduce.write_json(second / "identity.json", {"source_identity": "new source"})
            with self.assertRaisesRegex(ValueError, "Checked-in baseline differs"):
                reproduce.compare(first, second, baseline)


if __name__ == "__main__":
    unittest.main()
