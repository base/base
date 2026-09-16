#!/usr/bin/env python3
"""Exercise release tag creation against a local bare Git remote without network access."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("create-tag.sh").resolve()
RELEASE_BRANCH = "releases/v1.4.0"


class CreateTagTests(unittest.TestCase):
    def setUp(self):
        temporary_directory = tempfile.TemporaryDirectory(prefix="create-tag-test-")
        self.addCleanup(temporary_directory.cleanup)
        self.directory = Path(temporary_directory.name)
        self.repository = self.directory / "repository"
        self.remote = self.directory / "remote.git"
        self.environment = {
            **os.environ,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        self.repository.mkdir()
        self.git("init", "--bare", str(self.remote))
        self.git("init", "--initial-branch", RELEASE_BRANCH)
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", "release-test@example.com")
        self.git("remote", "add", "origin", str(self.remote))
        self.initial_commit = self.commit("Initial release commit")

    def git(self, *arguments, remote=False):
        return subprocess.run(
            ["git", *arguments],
            cwd=self.remote if remote else self.repository,
            env=self.environment,
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()

    def commit(self, message):
        self.git("commit", "--allow-empty", "-m", message)
        self.git("push", "origin", RELEASE_BRANCH)
        return self.git("rev-parse", "HEAD")

    def create_tag(self, release_type="rc", branch=RELEASE_BRANCH, check=True):
        with tempfile.TemporaryDirectory(dir=self.directory) as runner_temp:
            result = subprocess.run(
                ["bash", str(SCRIPT), branch, release_type],
                cwd=self.repository,
                env={**self.environment, "RUNNER_TEMP": runner_temp},
                text=True,
                capture_output=True,
                check=check,
            )
            output = Path(runner_temp) / "release_tag"
            tag = output.read_text().strip() if output.exists() else None
        return result, tag

    def assert_remote_tag(self, tag, commit):
        self.assertEqual(self.git("rev-parse", f"refs/tags/{tag}^{{}}", remote=True), commit)

    def test_creates_first_rc_at_triggering_commit(self):
        _, tag = self.create_tag()

        self.assertEqual(tag, "v1.4.0-rc.1")
        self.assert_remote_tag(tag, self.initial_commit)

    def test_new_commit_gets_next_rc_without_moving_previous_tag(self):
        _, first_tag = self.create_tag()
        next_commit = self.commit("Next release commit")

        _, next_tag = self.create_tag()

        self.assertEqual(next_tag, "v1.4.0-rc.2")
        self.assert_remote_tag(first_tag, self.initial_commit)
        self.assert_remote_tag(next_tag, next_commit)

    def test_retry_reuses_original_rc_after_newer_commits_and_tags(self):
        _, original_tag = self.create_tag()
        self.commit("Next release commit")
        self.create_tag()
        self.commit("Branch advances again")
        original_tags = self.git("tag", "--list", remote=True)
        original_tag_object = self.git("rev-parse", f"refs/tags/{original_tag}", remote=True)
        self.git("checkout", "--detach", self.initial_commit)

        _, retried_tag = self.create_tag()

        self.assertEqual(retried_tag, original_tag)
        self.assertEqual(self.git("tag", "--list", remote=True), original_tags)
        self.assertEqual(
            self.git("rev-parse", f"refs/tags/{retried_tag}", remote=True), original_tag_object
        )

    def test_rc_reuse_does_not_cross_release_versions(self):
        self.create_tag(branch="releases/v1.3.0")

        _, tag = self.create_tag()

        self.assertEqual(tag, "v1.4.0-rc.1")
        self.assert_remote_tag(tag, self.initial_commit)

    def test_final_tag_can_be_created_and_retried_at_same_commit(self):
        _, tag = self.create_tag("final")
        original_tag_object = self.git("rev-parse", f"refs/tags/{tag}", remote=True)

        _, retried_tag = self.create_tag("final")

        self.assertEqual(tag, "v1.4.0")
        self.assertEqual(retried_tag, tag)
        self.assert_remote_tag(tag, self.initial_commit)
        self.assertEqual(
            self.git("rev-parse", f"refs/tags/{tag}", remote=True), original_tag_object
        )

    def test_final_tag_cannot_move_to_different_commit(self):
        _, tag = self.create_tag("final")
        self.commit("Later release commit")

        result, output_tag = self.create_tag("final", check=False)

        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(output_tag)
        self.assert_remote_tag(tag, self.initial_commit)


if __name__ == "__main__":
    unittest.main()
