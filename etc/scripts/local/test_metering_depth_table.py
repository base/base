#!/usr/bin/env python3
"""Checks metering-depth-table.py's proof parsing and table output: python3 etc/scripts/local/test_metering_depth_table.py"""

import importlib.util
import pathlib
import unittest

SPEC = importlib.util.spec_from_file_location(
    "metering_depth_table", pathlib.Path(__file__).with_name("metering-depth-table.py")
)
table = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(table)

# A branch node: 16 child hashes (one present) and an empty value.
BRANCH = "0x" + (bytes([0xF8, 0x31, 0xA0]) + b"\x11" * 32 + b"\x80" * 16).hex()
# A leaf node: a two-item list of a short path and a short value.
LEAF = "0xc4823a0101"
EMPTY = table.EMPTY_STORAGE_ROOT


def proof(branch_counts, storage_hash="0x" + "ab" * 32):
    return {
        "accountProof": [BRANCH] * 9,
        "storageHash": storage_hash,
        "storageProof": [{"proof": [BRANCH] * count + [LEAF]} for count in branch_counts],
    }


class MeteringDepthTableTest(unittest.TestCase):
    def test_counts_branch_nodes_on_each_proof_path(self):
        self.assertEqual(table.rlp_list_items(BRANCH), 17)
        self.assertEqual(table.rlp_list_items(LEAF), 2)
        record = table.probe_record("0xAA", proof([6, 7, 6, 7, 7, 7]))
        self.assertEqual(record["br"], [6, 7, 6, 7, 7, 7])
        self.assertEqual(record["br_med"], 7)
        self.assertEqual(record["a"], "0xaa")
        self.assertFalse(record["empty"])

    def test_rounds_fractional_medians_half_to_even_and_skips_empty_storage(self):
        def depth(branches, storage_hash="0x" + "ab" * 32):
            return table.depth_class(table.probe_record("0x01", proof(branches, storage_hash)))

        self.assertEqual(depth([4, 4, 4, 5, 5, 5]), 4)
        self.assertEqual(depth([5, 5, 5, 6, 6, 6]), 6)
        self.assertIsNone(depth([0] * 6, EMPTY))
        self.assertIsNone(table.depth_class({"a": "0x01", "error": "timeout"}))

    def test_builds_sorted_table_above_min_depth_keeping_previous_class_on_failure(self):
        records = [
            {"a": "0xBB", "br_med": 8.0, "empty": False},
            {"a": "0xaa", "br_med": 8.0, "empty": False},
            {"a": "0xcc", "br_med": 3.0, "empty": False},
            {"a": "0xdd", "br_med": 5.0, "empty": False},
            {"a": "0xee", "error": "timeout"},
        ]
        built = table.build_table(records, 4, 2100, previous={"0xee": 7, "0xdd": 6})
        self.assertEqual(
            built,
            {
                "nodeBudget": 2100,
                "classes": [
                    {"depth": 8, "contracts": ["0xaa", "0xbb"]},
                    {"depth": 7, "contracts": ["0xee"]},
                    {"depth": 5, "contracts": ["0xdd"]},
                ],
            },
        )
        self.assertEqual(
            table.class_changes({"0xee": 7, "0xdd": 6}, table.table_classes(built)),
            [("0xaa", None, 8), ("0xbb", None, 8), ("0xdd", 6, 5)],
        )

    def test_default_min_depth_is_the_shallowest_class_with_a_cap(self):
        self.assertEqual(table.default_min_depth(2100), 4)
        self.assertEqual(table.default_min_depth(273), 4)
        self.assertEqual(table.default_min_depth(272), 3)
        self.assertEqual(table.default_min_depth(5000), 5)


if __name__ == "__main__":
    unittest.main()
