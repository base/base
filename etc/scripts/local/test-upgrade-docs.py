#!/usr/bin/env python3
"""Exercise the upgrade documentation CLI on disposable repositories."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("upgrade-docs.py")


class UpgradeDocsTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        self.write("etc/scripts/local/upgrade-docs.py", SCRIPT.read_text())
        self.write("crates/common/genesis/src/feature.rs", """
features! {
    Regolith {
        /// Include the deposit nonce in receipts.
        DepositNonce,
    },
}
""")
        self.write("crates/common/genesis/src/chain/upgrade.rs", """
    BaseUpgrade {
        Bedrock,
        Regolith,
    }
""")
        for adapter in (
            "genesis/src/rollup.rs", "chains/src/upgrades.rs", "evm/src/spec.rs", "evm2/src/spec.rs",
            "chains/src/upgrade.rs", "genesis/src/schedule.rs", "evm2/src/transition.rs",
        ):
            self.write("crates/common/" + adapter, "// schedule adapter\n")
        self.write("crates/execution/receipt/src/lib.rs", """
fn receipt() { if enabled(BaseFeature::DepositNonce) { include_nonce(); } }
#[cfg(test)]
mod tests { fn fixture() {} }
""")
        self.write("docs/guides/UPGRADES.md", """# Upgrades
Baseline protocol.
<!-- FEATURES:Bedrock:START -->
<!-- FEATURES:Bedrock:END -->
Deposit nonce behavior.
<!-- FEATURES:Regolith:START -->
<!-- FEATURES:Regolith:END -->
""")

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def run_cli(self, *args, success=True):
        result = subprocess.run(
            ["python3", "etc/scripts/local/upgrade-docs.py", *args],
            cwd=self.root, capture_output=True, text=True, timeout=5,
        )
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def sync(self):
        self.run_cli("sync")
        self.run_cli("check")

    def test_implementation_edits_do_not_require_documentation_changes(self):
        self.sync()
        guide = self.root / "docs/guides/UPGRADES.md"
        original_guide = guide.read_text()
        consumer = self.root / "crates/execution/receipt/src/lib.rs"
        # Formatting, logging, and behavior changes behind an unchanged gate are
        # intentionally outside structural validation; protocol review covers semantics.
        consumer.write_text(consumer.read_text().replace("include_nonce();", "\n    log(); include_nonce();\n"))
        self.run_cli("check")
        consumer.write_text(consumer.read_text().replace("include_nonce()", "omit_nonce()"))
        self.run_cli("check")
        consumer.write_text(consumer.read_text().replace("fixture()", "different_fixture()"))
        self.run_cli("check")
        for path in (
            "genesis/src/chain/upgrade.rs",
            "chains/src/upgrade.rs",
            "genesis/src/schedule.rs",
            "evm2/src/transition.rs",
        ):
            with self.subTest(path=path):
                schedule = self.root / "crates/common" / path
                schedule.write_text(schedule.read_text() + "\nfn activation() { configured_time() }\n")
                self.run_cli("check")
        self.sync()
        self.assertEqual(guide.read_text(), original_guide)

    def test_registry_mapping_and_description_changes_require_sync(self):
        self.sync()
        registry = self.root / "crates/common/genesis/src/feature.rs"
        for old, new in (
            ("Regolith {", "Bedrock {"),
            ("Include the deposit nonce in receipts.", "Include the sender nonce for deposits."),
        ):
            with self.subTest(change=new):
                registry.write_text(registry.read_text().replace(old, new))
                self.assertIn("inventory is stale", self.run_cli("check", success=False))
                self.sync()
        guide = (self.root / "docs/guides/UPGRADES.md").read_text()
        bedrock = guide.split("<!-- FEATURES:Bedrock:START -->")[1].split("<!-- FEATURES:Bedrock:END -->")[0]
        regolith = guide.split("<!-- FEATURES:Regolith:START -->")[1].split("<!-- FEATURES:Regolith:END -->")[0]
        self.assertIn("`DepositNonce` | Include the sender nonce for deposits.", bedrock)
        self.assertNotIn("DepositNonce", regolith)
        self.assertIn("Deposit nonce behavior.", guide)

    def test_grouped_registry_preserves_feature_ownership_and_descriptions(self):
        self.write("crates/common/genesis/src/feature.rs", """
features! {
    Bedrock {
        /// Require advancing timestamps.
        MonotonicBlockTimestamps,
    },
    Regolith {
        /// Enforce deposit gas limits.
        DepositGasAccounting,
        /// Include the sender nonce
        /// in deposit receipts.
        DepositNonce,
    },
}
""")
        self.write("crates/execution/receipt/src/lib.rs", """
fn receipt() {
    enabled(BaseFeature::MonotonicBlockTimestamps);
    enabled(BaseFeature::DepositGasAccounting);
    enabled(BaseFeature::DepositNonce);
}
""")
        self.sync()
        guide = (self.root / "docs/guides/UPGRADES.md").read_text()
        bedrock = guide.split("<!-- FEATURES:Bedrock:START -->")[1].split("<!-- FEATURES:Bedrock:END -->")[0]
        regolith = guide.split("<!-- FEATURES:Regolith:START -->")[1].split("<!-- FEATURES:Regolith:END -->")[0]
        self.assertIn("`MonotonicBlockTimestamps` | Require advancing timestamps.", bedrock)
        self.assertNotIn("Deposit", bedrock)
        self.assertIn("`DepositGasAccounting` | Enforce deposit gas limits.", regolith)
        self.assertIn("`DepositNonce` | Include the sender nonce in deposit receipts.", regolith)
        self.assertNotIn("MonotonicBlockTimestamps", regolith)

    def test_malformed_registry_is_not_silently_truncated(self):
        self.sync()
        registry = self.root / "crates/common/genesis/src/feature.rs"
        original = registry.read_text()
        for old, new in (
            ("DepositNonce,", "DepositNonce,\n        UndocumentedFeature,"),
            ("DepositNonce,", "DepositNonce => Regolith,"),
            ("    },", "    },\n    Bedrock {\n    },"),
            ("    },", ""),
            ("    },", "        /// Duplicate.\n        DepositNonce,\n    },"),
        ):
            with self.subTest(change=new):
                registry.write_text(original.replace(old, new))
                self.assertIn("malformed", self.run_cli("check", success=False))

    def test_attribute_like_source_does_not_stall_inventory(self):
        consumer = self.root / "crates/execution/receipt/src/lib.rs"
        repeated_attributes = "#[cfg(test)]#[" + "]#[" * 32 + "!"
        consumer.write_text(
            f'const ATTRIBUTES: &str = "{repeated_attributes}";\n' + consumer.read_text()
        )
        self.sync()

    def test_url_literal_does_not_hide_gate_and_preserves_line_numbers(self):
        self.sync()
        path = "crates/execution/receipt/src/lib.rs"
        consumer = self.root / path
        consumer.write_text(
            '/* first\nsecond */\nfn bypass() { let url = "https://base.org"; '
            'cfg.is_regolith_active(10); }\n' + consumer.read_text()
        )
        output = self.run_cli("check", success=False)
        self.assertIn(f"{path}:3: Base behavior must use a feature gate", output)

    def test_literals_and_comments_are_not_gates_or_feature_uses(self):
        self.sync()
        path = "crates/execution/receipt/src/examples.rs"
        text = "cfg.is_regolith_active(10); BaseFeature::Unknown; BaseFeature::DepositNonce"
        for source in (
            f"// {text}\n",
            f"/* {text} */\n",
            f"/* outer /* nested */ {text} */\n",
            f'const S: &str = "{text}";\n',
            f'const S: &str = r"{text}";\n',
            f'const S: &str = r###"a "# quote\n{text}"###;\n',
            f'const S: &[u8] = b"{text}";\n',
            f'const S: &[u8] = br##"{text}"##;\n',
            f'const S: &CStr = c"{text}";\n',
            f'const S: &CStr = cr#"{text}"#;\n',
        ):
            for contents in text.split("; "):
                with self.subTest(source=source, contents=contents):
                    self.write(path, source.replace(text, contents))
                    # Neither an unknown use nor an extra consumer should appear.
                    self.run_cli("check")
            with self.subTest(source=source, subsequent_gate=True):
                self.write(path, source + "cfg.is_regolith_active(10);\n")
                output = self.run_cli("check", success=False)
                line = source.count("\n") + 1
                self.assertIn(f"{path}:{line}: Base behavior must use a feature gate", output)

    def test_removed_consumer_cannot_be_replaced_by_comments_or_literals(self):
        self.sync()
        path = "crates/execution/receipt/src/lib.rs"
        for source in (
            "// BaseFeature::DepositNonce\n",
            "/* BaseFeature::DepositNonce */\n",
            'const S: &str = "BaseFeature::DepositNonce";\n',
            'const S: &str = r#"BaseFeature::DepositNonce"#;\n',
        ):
            with self.subTest(source=source):
                self.write(path, source)
                self.assertIn("DepositNonce has no production consumers",
                              self.run_cli("check", success=False))

    def test_literal_and_comment_test_modules_do_not_truncate_production(self):
        self.sync()
        path = "crates/execution/receipt/src/lib.rs"
        consumer = self.root / path
        original = consumer.read_text()
        fake_module = "\n#[cfg(test)]\nmod tests {}\n"
        for prefix in (
            f'const S: &str = "{fake_module}";\n',
            f'const S: &str = r##"{fake_module}"##;\n',
            f"/* {fake_module} */\n",
            f"/* outer /* inner */ {fake_module} */\n",
            '#[cfg(feature = "test")]\nmod tests {}\n',
        ):
            with self.subTest(prefix=prefix):
                consumer.write_text(prefix + original)
                self.run_cli("check")
                consumer.write_text(prefix + "cfg.is_regolith_active(10);\n" + original)
                output = self.run_cli("check", success=False)
                line = prefix.count("\n") + 1
                self.assertIn(f"{path}:{line}: Base behavior must use a feature gate", output)

    def test_lifetimes_and_escaped_chars_do_not_hide_subsequent_gates(self):
        self.sync()
        path = "crates/execution/receipt/src/lib.rs"
        consumer = self.root / path
        original = consumer.read_text()
        for prefix in (
            "fn lifetime<'a>(x: &'a str) { let _ = x; }",
            "fn lifetime(x: &'static str, y: &'_ str) {}",
            "'label: loop { break 'label; }",
            r"let chars = ('\'', '\\', '\n', '\u{1f600}', '🦀');",
            r"let bytes = (b'\'', b'\\', b'\x22', b'\n');",
            r'''let quotes = ('"', b'"'); let s = "escaped \" quote";''',
        ):
            with self.subTest(prefix=prefix):
                consumer.write_text(prefix + " cfg.is_regolith_active(10);\n" + original)
                output = self.run_cli("check", success=False)
                self.assertIn(f"{path}:1: Base behavior must use a feature gate", output)

    def test_inventory_tracks_new_and_deleted_consumers(self):
        self.sync()
        self.write("crates/proof/receipt/src/lib.rs", """
fn proof() { BaseFeature::activation_states([BaseFeature::DepositNonce], enabled); }
""")
        self.assertIn("inventory is stale", self.run_cli("check", success=False))
        self.sync()
        guide = (self.root / "docs/guides/UPGRADES.md").read_text()
        self.assertIn("../../crates/proof/receipt/src/lib.rs", guide)
        shutil.rmtree(self.root / "crates/proof")
        self.assertIn("broken code/documentation link", self.run_cli("check", success=False))
        self.sync()

    def test_unknown_feature_missing_upgrade_and_dead_feature_fail(self):
        self.sync()
        self.write("crates/execution/receipt/src/lib.rs", "fn receipt() { enabled(BaseFeature::Unknown); }")
        self.assertIn("unregistered feature", self.run_cli("check", success=False))
        self.write("crates/execution/receipt/src/lib.rs", "fn receipt() {}")
        self.assertIn("no production consumers", self.run_cli("check", success=False))
        self.write("crates/execution/receipt/src/lib.rs", "fn receipt() { enabled(BaseFeature::DepositNonce); }")
        # An early test helper declaration must not hide subsequent production code.
        consumer = self.root / "crates/execution/receipt/src/lib.rs"
        consumer.write_text("#[cfg(test)]\nmod mocks;\n" + consumer.read_text())
        self.run_cli("check")
        consumer.write_text(consumer.read_text().replace("DepositNonce", "Unknown"))
        self.assertIn("unregistered feature", self.run_cli("check", success=False))
        consumer.write_text(consumer.read_text().replace("Unknown", "DepositNonce"))
        guide = self.root / "docs/guides/UPGRADES.md"
        original = guide.read_text()
        guide.write_text(original + "\n<!-- FEATURES:Bedrock:START -->\n<!-- FEATURES:Bedrock:END -->\n")
        self.assertIn("exactly one FEATURES block", self.run_cli("check", success=False))
        guide.write_text(original)
        upgrade = self.root / "crates/common/genesis/src/chain/upgrade.rs"
        upgrade.write_text(upgrade.read_text().replace("        Regolith,", "        Regolith,\n        Canyon,"))
        self.assertIn("exactly one FEATURES block", self.run_cli("check", success=False))

    def test_named_behavior_gates_are_rejected_but_ethereum_and_tests_are_allowed(self):
        self.sync()
        consumer = self.root / "crates/execution/receipt/src/lib.rs"
        original = consumer.read_text()
        for gate in (
            "cfg.is_regolith_active(10)",
            "spec.is_enabled_in(BaseUpgrade::Regolith)",
            "upgrade >= BaseUpgrade::Regolith",
        ):
            consumer.write_text(f"fn bypass() {{ {gate}; }}\n" + original)
            self.assertIn("must use a feature gate", self.run_cli("check", success=False))
        consumer.write_text(original.replace(
            "fn fixture() {}", "fn fixture() { cfg.is_regolith_active(0); }"
        ).replace("#[cfg(test)]", '#[cfg(all(test, feature = "std"))]\n#[rustfmt::skip]')
            .replace("mod tests", "pub(super) mod tests"))
        consumer.write_text("fn ethereum() { spec.is_enabled_in(SpecId::OSAKA); }\n" + consumer.read_text())
        self.run_cli("check")


if __name__ == "__main__":
    unittest.main()
