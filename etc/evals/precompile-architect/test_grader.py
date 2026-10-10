#!/usr/bin/env python3
"""Self-tests for the grader: known-wrong plans must fail, valid variation must not.

Run: python3 etc/evals/precompile-architect/test_grader.py
"""

import copy
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from evallib import CASES, extract_plan, load_json  # noqa: E402
from grade import grade_plan  # noqa: E402

S = "crates/common/precompiles/src"


def case(case_id):
    return load_json(CASES / f"{case_id}.json")


def judged(case_data, overrides=None):
    items = {item["id"]: {"verdict": "yes"} for item in case_data["rubric"]}
    for item_id, verdict in (overrides or {}).items():
        items[item_id] = {"verdict": verdict}
    return {"items": items}


def grade(case_id, mutate=lambda plan: None, judge_overrides=None):
    data = case(case_id)
    plan = copy.deepcopy(data["expected"])
    mutate(plan)
    return grade_plan(data, plan, judged(data, judge_overrides))


class ReferenceSolutions(unittest.TestCase):
    def test_every_reference_plan_passes_with_full_partial_credit(self):
        for path in sorted(CASES.glob("*.json")):
            data = load_json(path)
            with self.subTest(case=data["id"]):
                result = grade_plan(data, data["expected"], judged(data))
                self.assertTrue(result["pass"], result)
                self.assertEqual(result["partial_score"], 1.0)

    def test_every_case_has_a_critical_item(self):
        for path in sorted(CASES.glob("*.json")):
            self.assertTrue(any(i["critical"] for i in load_json(path)["rubric"]), path.name)


class HardFails(unittest.TestCase):
    def test_proceeding_on_a_reject_case(self):
        def backport(plan):
            plan.update(verdict="proceed", approach="edit_in_place")
            plan["files"]["modify"] = [f"{S}/b20_asset/logic/v2.rs"]
        result = grade("head-reject-backport-credit-recipient", backport)
        self.assertFalse(result["pass"])
        self.assertIn("verdict_mismatch", result["hard_fails"])
        self.assertIn("touches_frozen", result["hard_fails"])

    def test_rejecting_a_safe_request(self):
        result = grade("denim-scaffold-asset-stablecoin-v3", lambda plan: plan.update(verdict="reject", approach="none"))
        self.assertIn("verdict_mismatch", result["hard_fails"])

    def test_touching_a_frozen_file(self):
        result = grade("denim-scaffold-asset-stablecoin-v3",
                       lambda plan: plan["files"]["modify"].append(f"{S}/b20_asset/logic/v2.rs"))
        self.assertEqual(result["hard_fails"], ["touches_frozen"])

    def test_omitting_an_expected_frozen_version(self):
        result = grade("denim-scaffold-asset-stablecoin-v3", lambda plan: plan["versions"].update(frozen=["V1"]))
        self.assertEqual(result["hard_fails"], ["missing_frozen_version"])


    def test_reject_without_frozen_list_is_not_a_hard_fail(self):
        result = grade("head-reject-delete-v1", lambda plan: plan["versions"].update(frozen=[]))
        self.assertTrue(result["pass"])
        self.assertLess(result["partial"]["frozen_coverage"], 1.0)


class ExactMatch(unittest.TestCase):
    def test_new_version_where_editing_unshipped_v3_is_right(self):
        def v4(plan):
            plan.update(approach="new_logic_version")
            plan["versions"] = {"create": ["V4"], "modify": [], "frozen": ["V1", "V2", "V3"]}
        result = grade("denim-executor-policy-in-v3", v4)
        self.assertFalse(result["pass"])
        self.assertFalse(result["exact"]["approach"])

    def test_wrong_activation_fork(self):
        self.assertFalse(grade("pre-beryl-mint-pause-before-policy", lambda plan: plan.update(activation_fork=None))["pass"])

    def test_missing_symmetric_module(self):
        result = grade("denim-scaffold-asset-stablecoin-v3", lambda plan: plan.update(symmetric_modules=["b20_asset"]))
        self.assertFalse(result["exact"]["symmetric_modules"])

    def test_extra_symmetric_module_is_allowed(self):
        result = grade("denim-scaffold-asset-stablecoin-v3",
                       lambda plan: plan.update(symmetric_modules=["b20_asset", "b20_stablecoin", "policy"]))
        self.assertTrue(result["pass"])

    def test_accepted_alternative(self):
        result = grade("zeronet-factory-keccak-metering", lambda plan: plan.update(approach="fork_gate"))
        self.assertTrue(result["pass"])
        self.assertEqual(result["matched_answer"], 1)

    def test_alternative_must_match_all_its_fields(self):
        result = grade("cobalt-tristate-announce-decode", lambda plan: plan.update(approach="edit_in_place"))
        self.assertFalse(result["pass"])


class Rubric(unittest.TestCase):
    def test_critical_item_answered_no_fails(self):
        self.assertFalse(grade("beryl-borrowed-announce-decode", judge_overrides={"byte_identical": "no"})["pass"])

    def test_critical_item_answered_unknown_fails(self):
        result = grade("beryl-borrowed-announce-decode", judge_overrides={"byte_identical": "unknown"})
        self.assertFalse(result["pass"])
        self.assertEqual(result["rubric"]["unknown"], 1)

    def test_supporting_item_only_costs_partial_credit(self):
        result = grade("beryl-borrowed-announce-decode", judge_overrides={"no_metering": "unknown"})
        self.assertTrue(result["pass"])
        self.assertLess(result["partial_score"], 1.0)

    def test_no_judge_means_no_pass(self):
        data = case("beryl-borrowed-announce-decode")
        result = grade_plan(data, data["expected"])
        self.assertTrue(result["code_pass"])
        self.assertFalse(result["pass"])


class PartialCreditOnly(unittest.TestCase):
    def test_wrong_surfaces_do_not_fail(self):
        result = grade("cobalt-meter-permit", lambda plan: plan["surfaces"].update(gas=False))
        self.assertTrue(result["pass"])
        self.assertLess(result["partial"]["surfaces"], 1.0)

    def test_stray_file_costs_precision_only(self):
        result = grade("denim-scaffold-asset-stablecoin-v3",
                       lambda plan: plan["files"]["modify"].append("crates/execution/evm/src/lib.rs"))
        self.assertTrue(result["pass"])
        self.assertLess(result["partial"]["file_precision"], 1.0)

    def test_formatting_and_plumbing_are_free(self):
        def reformat(plan):
            plan["versions"]["create"] = ["v3"]
            plan["files"]["create"] = ["./" + p for p in plan["files"]["create"]]
            plan["files"]["modify"] += [f"{S}/b20_asset/mod.rs", "crates/common/precompiles/tests/b20_asset_v3_golden.rs"]
        self.assertEqual(grade("denim-scaffold-asset-stablecoin-v3", reformat)["partial_score"], 1.0)


class Parsing(unittest.TestCase):
    def test_reject_with_omitted_empty_fields_is_repaired(self):
        text = '```json\n{"verdict": "reject", "approach": "none", "versions": {"frozen": ["V1", "V2"]}, "files": {}, "must_not_modify": []}\n```'
        plan, repairs, error = extract_plan(text)
        self.assertIsNone(error)
        self.assertTrue(repairs)
        data = case("head-reject-delete-v1")
        self.assertTrue(grade_plan(data, plan, judged(data))["pass"])

    def test_missing_verdict_is_a_format_failure(self):
        plan, _, error = extract_plan('```json\n{"approach": "none"}\n```')
        self.assertIsNone(plan)
        self.assertIsNotNone(error)


if __name__ == "__main__":
    unittest.main(verbosity=1)
