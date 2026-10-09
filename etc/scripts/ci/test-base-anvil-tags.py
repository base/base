#!/usr/bin/env python3
"""Exercise the package workflow's tag outputs without building or publishing images."""

from pathlib import Path
import subprocess
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[3]
IMAGE = "ghcr.io/base/base-anvil"
SHA = "d1cd95c42902d356dc95a15b79902bb26a4673e6"
DATE = "20261002"


class PackageTagsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        source = (ROOT / ".depot/workflows/base-anvil-package.yml").read_text()
        step = source.split("      - name: Determine package tags\n", 1)[1].split(
            "      - name: Create manifest list and push\n", 1
        )[0]
        script = textwrap.dedent(step.split("        run: |\n", 1)[1])
        # Only the commit-date lookup is stubbed; all tag decisions run in Bash.
        cls.script = f"git() {{ printf '%s\\n' {DATE}; }}\n" + script

    def package_tags(self, **overrides):
        env = {
            "PATH": "/usr/bin:/bin",
            "GITHUB_SHA": SHA,
            "GITHUB_REF_NAME": "main",
            "BASE_ANVIL_REF": "98e7839c65f64aee9627b69a9b98b79afaeb1fae",
            "BASE_STD_REF": "main",
            "BASE_UPGRADE": "denim",
            "REGISTRY_IMAGE": IMAGE,
            "GITHUB_OUTPUT": "/dev/stdout",
            **overrides,
        }
        result = subprocess.run(
            ["/bin/bash", "--noprofile", "--norc", "-c", self.script],
            cwd=ROOT, env=env, text=True, capture_output=True, check=True,
        )
        primary = result.stdout.split("primary_tag=", 1)[1].splitlines()[0]
        tags = result.stdout.split("docker_tags<<EOF\n", 1)[1].split(
            "\nEOF", 1
        )[0].splitlines()
        self.assertIn(f"{IMAGE}:{primary}", tags, "inspection must use a published tag")
        return primary, tags

    def test_default_main_preserves_canonical_and_floating_tags(self):
        primary, tags = self.package_tags()
        self.assertEqual(primary, f"sha-{SHA}")
        self.assertIn(f"{IMAGE}:sha-{SHA}", tags)
        self.assertIn(f"{IMAGE}:main", tags)
        self.assertIn(f"{IMAGE}:main-{DATE}-{SHA[:12]}", tags)

    def test_custom_inputs_cannot_replace_default_tags(self):
        for name, value in (
            ("BASE_ANVIL_REF", "alternate-anvil-ref"),
            ("BASE_STD_REF", "alternate-std-ref"),
            ("BASE_UPGRADE", "azul"),
        ):
            with self.subTest(input=name):
                primary, tags = self.package_tags(**{name: value})
                self.assertTrue(tags, "custom builds must still publish an image")
                self.assertNotIn(f"{IMAGE}:sha-{SHA}", tags)
                self.assertNotIn(f"{IMAGE}:main", tags)
                self.assertNotIn(f"{IMAGE}:main-{DATE}-{SHA[:12]}", tags)
                self.assertEqual(primary, f"manual-{DATE}-{SHA[:12]}")

    def test_branch_tags_remain_publishable(self):
        for branch, prefix in (("Feature/Custom", "feature-custom"), ("---", "ref")):
            for anvil_ref in (
                "98e7839c65f64aee9627b69a9b98b79afaeb1fae", "alternate-anvil-ref"
            ):
                with self.subTest(branch=branch, anvil_ref=anvil_ref):
                    primary, tags = self.package_tags(
                        GITHUB_REF_NAME=branch, BASE_ANVIL_REF=anvil_ref,
                    )
                    self.assertEqual(primary, f"{prefix}-{DATE}-{SHA[:12]}")
                    self.assertNotIn(f"{IMAGE}:sha-{SHA}", tags)
                    self.assertNotIn(f"{IMAGE}:main", tags)


if __name__ == "__main__":
    unittest.main()
