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
    def test_state_cannot_import_rpc_servers_through_any_dependency_kind(self):
        for kind in [None, "dev", "build"]:
            for optional in [False, True]:
                for target in [None, 'cfg(unix)']:
                    with self.subTest(kind=kind, optional=optional, target=target):
                        errors = check(
                            package("crates/execution/state/provider", [dependency(
                                "crates/execution/rpc", kind, optional, target)]),
                            package("crates/execution/rpc"))
                        self.assertTrue(any(
                            "execution/state cannot depend on execution/rpc" in e for e in errors))

    def test_execution_lower_layers_cannot_import_their_consumers(self):
        # Representative architectural inversions, rather than a copy of the rule table.
        for source, target in [
            ("state/types", "txpool"),
            ("state/indexer", "payload"),
            ("evm/runtime", "rpc"),
            ("evm/blocks", "engine/driver"),
            ("network/wire", "rpc"),
            ("network/service", "sync"),
            ("txpool", "payload"),
            ("payload", "rpc"),
            ("payload", "engine/driver"),
            ("sync", "rpc"),
            ("rpc", "engine/driver"),
        ]:
            for kind in [None, "dev", "build"]:
                with self.subTest(source=source, target=target, kind=kind):
                    self.assertTrue(check(
                        package("crates/execution/" + source, [dependency("crates/execution/" + target, kind)]),
                        package("crates/execution/" + target)))

    def test_execution_consumers_may_use_lower_layers_and_shared_schemas(self):
        for source, target in [
            ("execution/rpc", "execution/state/provider"),
            ("execution/rpc", "execution/payload"),
            ("execution/payload", "execution/txpool"),
            ("execution/network/service", "execution/txpool"),
            ("execution/txpool", "execution/network/wire"),
            ("execution/evm/blocks", "execution/state/types"),
            ("execution/state/provider", "execution/evm/runtime"),
            ("execution/state/types", "common/types/rpc"),
        ]:
            with self.subTest(source=source, target=target):
                self.assertEqual([], check(
                    package("crates/" + source, [dependency("crates/" + target)]),
                    package("crates/" + target)))

    def test_txpool_rpc_dependency_is_only_for_integration_tests(self):
        for kind, valid in [(None, False), ("build", False), ("dev", True)]:
            for optional in [False, True]:
                with self.subTest(kind=kind, optional=optional):
                    errors = check(
                        package("crates/execution/txpool", [dependency(
                            "crates/execution/rpc", kind, optional, 'cfg(unix)')]),
                        package("crates/execution/rpc"))
                    self.assertEqual(not errors, valid)

    def test_foundational_storage_does_not_import_execution_services(self):
        for source in ["types", "database", "provider", "trie", "mdbx-sys"]:
            for target in ["engine/observers", "network/service", "sync"]:
                for kind in [None, "dev", "build"]:
                    with self.subTest(source=source, target=target, kind=kind):
                        self.assertTrue(check(
                            package("crates/execution/state/" + source, [dependency(
                                "crates/execution/" + target, kind, True, 'cfg(unix)')]),
                            package("crates/execution/" + target)))

    def test_state_service_and_maintenance_integration_boundaries_remain_valid(self):
        for source, target, kind in [
            ("state/indexer", "engine/observers", None),
            ("state/indexer", "network/service", None),
            ("state/operations", "sync", "dev"),
        ]:
            with self.subTest(source=source, target=target):
                self.assertEqual([], check(
                    package("crates/execution/" + source, [dependency(
                        "crates/execution/" + target, kind, optional=True)]),
                    package("crates/execution/" + target)))

    def test_execution_may_use_common(self):
        self.assertEqual([], check(
            package("crates/execution/evm/runtime", [dependency("crates/common/types/chain")]),
            package("crates/common/types/chain")))

    def test_common_must_not_use_execution_even_conditionally(self):
        for kind, optional, target in [(None, False, None), ("dev", False, None),
                                       ("build", False, None), (None, True, 'cfg(unix)')]:
            with self.subTest(kind=kind, optional=optional, target=target):
                self.assertTrue(check(
                    package("crates/common/types/chain", [dependency("crates/execution/evm/runtime", kind, optional, target)]),
                    package("crates/execution/evm/runtime")))

    def test_node_integration_tests_do_not_create_runtime_dependencies(self):
        for kind, valid in [("dev", True), (None, False), ("build", False)]:
            with self.subTest(kind=kind):
                errors = check(
                    package("crates/execution/payload", [dependency("crates/node/service", kind)]),
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
                    package("crates/execution/sync",
                            [dependency("crates/testing/support", optional=optional)], features),
                    package("crates/testing/support"))
                self.assertEqual(not errors, valid)

    def test_payload_outcomes_may_use_only_execution_data_dependencies(self):
        for target, kind, valid in [
            ("state/types", None, True), ("evm/runtime", None, True),
            ("state/provider", None, False), ("engine/driver", None, False),
            ("evm/runtime", "build", False),
        ]:
            with self.subTest(target=target, kind=kind):
                errors = check(
                    package("crates/common/types/payload", [dependency("crates/execution/" + target, kind)]),
                    package("crates/execution/" + target))
                self.assertEqual(not errors, valid)

    def test_node_cannot_use_deleted_debug_fixture_exception(self):
        self.assertTrue(check(
            package("crates/node/service", [dependency("crates/testing/debug-client")]),
            package("crates/testing/debug-client")))

    def test_library_name_matches_path(self):
        item = package("crates/execution/evm/runtime")
        item["name"] = "base-evm"
        self.assertTrue(check(item))

    def test_vendor_packages_are_rejected(self):
        self.assertTrue(check(package("vendor/old")))

    def test_package_cannot_also_be_group(self):
        self.assertTrue(check(package("crates/execution/evm"), package("crates/execution/evm/runtime")))


class SingletonLayoutTests(unittest.TestCase):
    def check_layout(self, *paths):
        packages = [package(path) for path in paths]
        return policy.singleton_violations({
            "workspace_root": "/repo", "packages": packages,
            "workspace_members": [p["id"] for p in packages],
        })

    def test_single_nested_crate_is_rejected(self):
        errors = self.check_layout("crates/execution/txpool/pool")
        self.assertEqual(1, len(errors))
        self.assertIn("must be flattened to crates/execution/txpool", errors[0])

    def test_single_crate_below_multiple_empty_groups_is_rejected(self):
        errors = self.check_layout("crates/execution/txpool/internal/pool")
        self.assertEqual(1, len(errors))
        self.assertIn("must be flattened to crates/execution/txpool", errors[0])

    def test_multiple_crates_keep_their_group(self):
        self.assertEqual([], self.check_layout(
            "crates/execution/evm/runtime", "crates/execution/evm/macros"))

    def test_separate_multi_crate_branches_are_not_singletons(self):
        self.assertEqual([], self.check_layout(
            "crates/execution/evm/runtime/core", "crates/execution/evm/runtime/state",
            "crates/execution/evm/macros/derive", "crates/execution/evm/macros/attribute"))

    def test_flat_libraries_and_binary_layouts_are_preserved(self):
        self.assertEqual([], self.check_layout(
            "crates/execution/txpool", "crates/testing/devnet", "bin/prover/service"))


if __name__ == "__main__":
    unittest.main()
