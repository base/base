#!/usr/bin/env python3
"""Checks mainnet-mix.py's class mapping, weights and prediction: python3 etc/scripts/local/test_mainnet_mix.py"""

import importlib.util
import os
import re
import unittest

spec = importlib.util.spec_from_file_location("mainnet_mix", os.path.join(os.path.dirname(__file__), "mainnet-mix.py"))
mix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mix)


def classes(**shares):
    return {kind: dict(tx_share=shares.get(kind, 0), gas_share=0, gas_per_tx=0) for kind in mix.CLASSES}


class MainnetMixTest(unittest.TestCase):
    def test_every_class_maps_to_known_stand_ins(self):
        self.assertEqual(set(mix.CLASS_MIX), set(mix.CLASSES))
        for kind, stand_ins in mix.CLASS_MIX.items():
            self.assertAlmostEqual(sum(stand_ins.values()), 1, msg=kind)
            self.assertLessEqual(set(stand_ins), set(mix.STAND_INS), kind)

    def test_usdc_authorization_classifies_as_x402(self):
        log = dict(address="0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", topics=[mix.AUTHORIZATION_USED])
        tx = dict(input="0xe3ee160e", to=log["address"])
        self.assertEqual(mix.classify(tx, dict(logs=[log], status="0x1")), "x402")

    def test_weights_split_classes_and_sum_to_scale(self):
        weight = mix.weights(classes(transfer=0.5, swap=0.3, liquidity=0.2))
        self.assertEqual(sum(weight.values()), mix.WEIGHT_SCALE)
        self.assertEqual(weight["transfer"], 500)
        # storage_heavy serves both swaps and liquidity.
        heavy = 300 * mix.CLASS_MIX["swap"]["storage_heavy"] + 200 * mix.CLASS_MIX["liquidity"]["storage_heavy"]
        self.assertAlmostEqual(weight["storage_heavy"], heavy, delta=1)

    def test_predict_averages_stand_ins_by_weight(self):
        transfer, heavy = mix.STAND_INS["transfer"][1], mix.STAND_INS["storage_heavy"][1]
        predicted = mix.predict(dict(transfer=3, storage_heavy=1))
        self.assertAlmostEqual(predicted["gas_per_tx"], (3 * transfer + heavy) / 4)
        self.assertAlmostEqual(predicted["heavy_tx_share"], 0.25)
        self.assertAlmostEqual(predicted["heavy_gas_share"], heavy / (3 * transfer + heavy))

    def test_render_writes_every_weight_once(self):
        stats = dict(start=1, end=2, blocks=2, start_time="", end_time="", start_est="", end_est="",
                     per_block=dict(gas=1, txs=1, transfer_events=0), classes=classes(**{k: 1 for k in mix.CLASSES}))
        targets = dict(metrics=dict(gas_per_tx=dict(target=180_000, tolerance=0.1)))
        rendered = mix.render(stats, targets)
        self.assertEqual(sum(map(int, re.findall(r"\{weight: (\d+),", rendered))), mix.WEIGHT_SCALE)
        self.assertEqual(rendered.count("type: uniswap_v3_liquidity"), 1)


if __name__ == "__main__":
    unittest.main()
