#!/usr/bin/env python3
"""Regression tests for the crate architecture policy."""
import importlib.util
from pathlib import Path
import unittest
import sys

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("crate_deps", Path(__file__).with_name("check-crate-deps.py"))
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


def package(path, dependencies=(), features=None):
    name = "base-" + path.removeprefix("crates/").replace("/", "-")
    return {"id": name, "name": name, "manifest_path": f"/repo/{path}/Cargo.toml",
            "dependencies": list(dependencies), "features": features or {}}


def dependency(path, kind=None, optional=False, target=None):
    return {"path": f"/repo/{path}", "kind": kind, "optional": optional, "target": target}


def check(*packages):
    return policy.violations({"workspace_root": "/repo", "packages": packages,
                             "workspace_members": [p["id"] for p in packages]})


class ArchitectureTests(unittest.TestCase):
    def test_execution_may_use_common(self):
        self.assertEqual([], check(
            package("crates/execution/evm/primitives", [dependency("crates/common/types/chain")]),
            package("crates/common/types/chain")))

    def test_common_must_not_use_execution_even_conditionally(self):
        for kind, optional, target in [(None, False, None), ("dev", False, None),
                                       ("build", False, None), (None, True, 'cfg(unix)')]:
            with self.subTest(kind=kind, optional=optional, target=target):
                self.assertTrue(check(
                    package("crates/common/types/chain", [dependency("crates/execution/evm/primitives", kind, optional, target)]),
                    package("crates/execution/evm/primitives")))

    def test_node_integration_tests_do_not_create_runtime_dependencies(self):
        for kind, valid in [("dev", True), (None, False), ("build", False)]:
            with self.subTest(kind=kind):
                errors = check(
                    package("crates/execution/payload/builder", [dependency("crates/node/service", kind)]),
                    package("crates/node/service"))
                self.assertEqual(not errors, valid)

    def test_testing_runtime_dependency_is_rejected(self):
        self.assertTrue(check(
            package("crates/execution/state/provider", [dependency("crates/testing/support")]),
            package("crates/testing/support")))

    def test_pipeline_test_helpers_require_optional_feature(self):
        for optional, features, valid in [
            (True, {"test-utils": ["dep:base-testing-support"]}, True),
            (False, {"test-utils": ["dep:base-testing-support"]}, False),
            (True, {}, False),
        ]:
            with self.subTest(optional=optional, features=features):
                errors = check(
                    package("crates/execution/sync/pipeline",
                            [dependency("crates/testing/support", optional=optional)], features),
                    package("crates/testing/support"))
                self.assertEqual(not errors, valid)

    def test_operational_debug_client_is_allowed(self):
        self.assertEqual([], check(
            package("crates/node/service", [dependency("crates/testing/debug-client")]),
            package("crates/testing/debug-client")))

    def test_library_name_matches_path(self):
        item = package("crates/execution/evm/primitives")
        item["name"] = "base-evm"
        self.assertTrue(check(item))

    def test_vendor_packages_are_rejected(self):
        self.assertTrue(check(package("vendor/old")))

    def test_package_cannot_also_be_group(self):
        self.assertTrue(check(package("crates/execution/evm"), package("crates/execution/evm/primitives")))


if __name__ == "__main__":
    unittest.main()
