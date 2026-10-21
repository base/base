#!/usr/bin/env python3
"""Check the dashboard's actual PromQL with the devnet's Prometheus image.

Run with Python 3 and Docker; no running devnet or Python dependencies required.
Temporary JSON documents are also valid YAML input for promtool.
"""

import json
from pathlib import Path
import re
import subprocess
import tempfile


DIRECTORY = Path(__file__).resolve().parent
PREFIX = "reth_base_upgrade_signal_"


def panels(dashboard):
    for panel in dashboard.get("panels", []):
        yield panel
        yield from panels(panel)


def interpolate(expression, defaults, **overrides):
    variables = {
        **defaults,
        "__rate_interval": "5m",
        "__interval": "1m",
        "__range": "10m",
        **overrides,
    }
    for name, value in variables.items():
        expression = expression.replace("${" + name + "}", value)
        expression = expression.replace("$" + name, value)
    if "$" in expression:
        raise ValueError(f"Unresolved Grafana variable: {expression}")
    return expression


def series(metric, value, *, instance="base-builder:7090", layer="el", upgrade="azul"):
    labels = {
        "job": "l2_builder",
        "instance": instance,
        "layer": layer,
        "upgrade": upgrade,
    }
    if upgrade is None:
        del labels["upgrade"]
    label_string = ",".join(f"{key}={json.dumps(value)}" for key, value in labels.items())
    return {"series": f"{PREFIX}{metric}{{{label_string}}}", "values": value}


def sample(labels, value):
    return {"labels": labels, "value": value}


def main():
    dashboard = json.loads((DIRECTORY / "dashboards/dynamic-upgrades.json").read_text())
    defaults = {variable["name"]: variable["allValue"]
                for variable in dashboard["templating"]["list"]}
    expressions = {
        (panel["title"], target["refId"]): target["expr"]
        for panel in panels(dashboard)
        for target in panel.get("targets", [])
        if "expr" in target
    }

    def query(title, **variables):
        return interpolate(expressions[(title, "A")], defaults, **variables)

    tests = []

    def case(name, inputs, *expectations):
        tests.append({
            "name": name,
            "interval": "1m",
            "input_series": inputs,
            "promql_expr_test": [
                {"expr": expression, "eval_time": "10m", "exp_samples": expected}
                for expression, expected in expectations
            ],
        })

    divergence = query("Activation disagreement · max − min")
    failures = query("Failing series")
    errors = query("L1 read errors")
    divergent_upgrades = query("Divergent upgrades")
    aligned = [
        series("activation_timestamp", "100+0x10"),
        series("activation_timestamp", "100+0x10", instance="base-client:8090"),
        series("activation_timestamp", "100+0x10", layer="cl"),
    ]
    case("missing telemetry is not agreement or zero failures", [],
         (divergence, []), (divergent_upgrades, []), (failures, []), (errors, []),
         (query("Reporting readers"), []))
    case("one reporter cannot establish agreement", aligned[:1],
         (divergence, []), (divergent_upgrades, []))
    case("aligned nodes and layers", aligned,
         (divergence, [sample('{upgrade="azul"}', 0)]))
    case("node disagreement is visible", [aligned[0],
         series("activation_timestamp", "140+0x10", instance="base-client:8090")],
         (divergence, [sample('{upgrade="azul"}', 40)]))
    case("layer disagreement is visible", [aligned[0],
         series("activation_timestamp", "160+0x10", layer="cl")],
         (divergence, [sample('{upgrade="azul"}', 60)]))
    case("never versus scheduled is disagreement", [aligned[0],
         series("activation_timestamp", "0+0x10", layer="cl")],
         (divergence, [sample('{upgrade="azul"}', 100)]))
    case("upgrade identities are compared separately", aligned + [
         series("activation_timestamp", "200+0x10", upgrade="beryl"),
         series("activation_timestamp", "220+0x10", upgrade="beryl", layer="cl")],
         (divergence, [sample('{upgrade="azul"}', 0), sample('{upgrade="beryl"}', 20)]),
         (divergent_upgrades, [sample("{}", 1)]))
    case("layer filter narrows comparisons", aligned,
         (query("Activation disagreement · max − min", layer="cl"), []))
    case("reported apply failures remain visible", [
         series("apply_failed", "0+0x10"),
         series("apply_failed", "1+0x10", layer="cl")],
         (failures, [sample("{}", 1)]))
    case("L1 errors are not multiplied by upgrade labels", [
         series("l1_read_errors_total", "0+60x10", upgrade=upgrade)
         for upgrade in ("azul", "beryl", "denim")],
         (errors, [sample('{instance="base-builder:7090",job="l2_builder",layer="el"}', 1)]))
    case("protocol-version divergence includes layers", [
         series("expected_protocol_version", "1+0x10"),
         series("expected_protocol_version", "2+0x10", layer="cl")],
         (query("Protocol-version disagreement · max − min"), [sample('{upgrade="azul"}', 1)]))
    case("coverage counts reporters not upgrade series", [
         series("last_l1_read_block", "100+0x10"),
         series("last_l1_read_block", "100+0x10", upgrade="beryl"),
         series("last_l1_read_block", "100+0x10", layer="cl")],
         (query("Reporting readers"), [sample("{}", 2)]))
    case("empty schedules have no upgrade label", [
         series("empty_schedule_reads_total", "0+60x10", upgrade=None)],
         (query("Empty L1 schedules", upgrade="azul"),
          [sample('{instance="base-builder:7090",job="l2_builder",layer="el"}', 1)]))
    case("failed scrapes are not healthy", [
         {"series": 'up{job="l2_builder",instance="base-builder:7090"}', "values": "1+0x10"},
         {"series": 'up{job="l2_client",instance="base-client:8090"}', "values": "0+0x10"},
         {"series": 'up{job="conductor",instance="op-conductor-0:6090"}', "values": "1+0x10"}],
         (query("Reachable nodes"), [sample("{}", 0.5)]))
    case("counter resets preserve the error rate", [
         series("l1_read_errors_total", "0 60 120 180 240 300 360 60 120 180 240")],
         (errors, [sample('{instance="base-builder:7090",job="l2_builder",layer="el"}', 1)]))
    first_event = series("l1_read_errors_total", "_ _ _ _ _ _ _ _ _ _ 1")
    case("first event remains visible before a rate can be calculated", [first_event],
         (errors, []),
         (query("Recorded per-upgrade event counters · Since process start"),
          [sample(first_event["series"], 1)]))

    # Parse every target, including panels that do not need semantic fixtures.
    rules = [{"record": f"dashboard_expression_{index}", "expr": interpolate(expression, defaults)}
             for index, expression in enumerate(expressions.values())]
    compose = (DIRECTORY.parents[2] / "docker/docker-compose.yml").read_text()
    image = re.search(r"image:\s*(prom/prometheus:[^\s]+)", compose).group(1)
    with tempfile.TemporaryDirectory(prefix="dynamic-upgrades-promql-") as temporary:
        directory = Path(temporary)
        (directory / "rules.json").write_text(json.dumps({"groups": [
            {"name": "dashboard", "rules": rules},
        ]}))
        (directory / "tests.json").write_text(json.dumps({
            "rule_files": ["rules.json"], "evaluation_interval": "1m", "tests": tests,
        }))
        command = ["docker", "run", "--rm", "--network", "none", "--entrypoint", "promtool",
                   "--volume", f"{directory}:/tests:ro", "--workdir", "/tests", image]
        subprocess.run(command + ["check", "rules", "rules.json"], check=True)
        subprocess.run(command + ["test", "rules", "tests.json"], check=True)
    print(f"Validated {len(expressions)} dashboard queries and {len(tests)} semantic scenarios.")


if __name__ == "__main__":
    main()
