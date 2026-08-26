#!/usr/bin/env python3
"""Render the Dowse canonical-replay benchmark as a dependency-free SVG."""

import json
import math
import statistics
import sys
from pathlib import Path
from xml.sax.saxutils import escape


INPUT_PATH = Path(sys.argv[1])
OUTPUT_PATH = Path(sys.argv[2])
WIDTH = 1440
HEIGHT = 1400
FONT = "Inter, ui-sans-serif, system-ui, -apple-system, sans-serif"


def attr(value):
    return escape(str(value), {'"': "&quot;"})


svg = []


def element(name, **attributes):
    svg.append(f"<{name} " + " ".join(f'{key.replace("_", "-")}="{attr(value)}"' for key, value in attributes.items()) + "/>")


def text(x, y, value, size=16, weight=400, fill="#17223b", anchor="start"):
    svg.append(
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(str(value))}</text>'
    )


with INPUT_PATH.open() as source:
    lines = [json.loads(line) for line in source]

metadata = lines[0]
records = [line for line in lines[1:] if line.get("kind") == "block"]
raw_times = [record["result"]["raw"]["executionTimeUs"] for record in records]
cached_times = [record["result"]["cached"]["executionTimeUs"] for record in records]
deltas = [(cached / raw - 1) * 100 for raw, cached in zip(raw_times, cached_times)]


def percentile(values, quantile):
    ordered = sorted(values)
    position = (len(ordered) - 1) * quantile
    lower = math.floor(position)
    upper = math.ceil(position)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def nice_axis(low, high, intervals):
    rough_step = (high - low) / intervals
    magnitude = 10 ** math.floor(math.log10(rough_step))
    scaled_step = rough_step / magnitude
    multiplier = (
        1 if scaled_step <= 1 else 2 if scaled_step <= 2 else 5 if scaled_step <= 5 else 10
    )
    step = multiplier * magnitude
    axis_low = math.floor(low / step) * step
    axis_high = math.ceil(high / step) * step
    ticks = [axis_low + index * step for index in range(round((axis_high - axis_low) / step) + 1)]
    return axis_low, axis_high, ticks


planning_times = []
prefetch_times = []
for record in records:
    result = record["result"]
    prefetch = result.get("prefetch", result.get("prewarm"))
    planning_times.append(prefetch["planningTimeUs"])
    prefetch_times.append(prefetch.get("prefetchTimeUs", prefetch.get("prewarmTimeUs")))

log_deltas = [math.log(cached / raw) for raw, cached in zip(raw_times, cached_times)]
serial_log_deltas = [
    math.log((cached + planning + prefetch) / raw)
    for raw, cached, planning, prefetch in zip(
        raw_times,
        cached_times,
        planning_times,
        prefetch_times,
    )
]
cached_first = [record["result"]["cachedFirst"] for record in records]


def order_balanced_change(log_changes):
    by_order = []
    for order in (True, False):
        group = [
            change
            for change, is_cached_first in zip(log_changes, cached_first)
            if is_cached_first == order
        ]
        by_order.append(statistics.fmean(group or log_changes))
    return (math.exp(statistics.fmean(by_order)) - 1) * 100, by_order


execution_delta, execution_by_order = order_balanced_change(log_deltas)
serial_delta, _ = order_balanced_change(serial_log_deltas)
first_replay_multiplier = math.exp((execution_by_order[0] - execution_by_order[1]) / 2)

provider = {}
for kind in ("account", "storage", "code"):
    field = f"{kind}Fetches"
    raw = sum(record["result"]["raw"]["stateProvider"][field] for record in records)
    cached = sum(record["result"]["cached"]["stateProvider"][field] for record in records)
    provider[kind] = (raw, cached, (cached / raw - 1) * 100)

svg.append(f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">')
element("rect", x=0, y=0, width=WIDTH, height=HEIGHT, fill="#f5f7fb")
text(50, 56, "Dowse state-prefetch replay benchmark", 30, 700, "#102044")
text(
    50,
    84,
    f'{len(records)} Base mainnet blocks · {sum(record.get("transactionCount", len(record["result"]["raw"].get("transactions", []))) for record in records):,} transactions · blocks {metadata.get("startBlock", metadata.get("start_block")):,}–{metadata.get("endBlock", metadata.get("end_block")):,}',
    15,
    400,
    "#66738f",
)

chips = [
    (
        "Order-balanced execution",
        f"{execution_delta:+.1f}%",
        "lower" if execution_delta < 0 else "higher",
        "#0052ff",
    ),
    ("Storage-provider reads", f"{provider['storage'][2]:.1f}%", "lower", "#00a37a"),
    (
        "Synchronous end-to-end",
        f"{serial_delta:+.1f}%",
        "slower" if serial_delta > 0 else "faster",
        "#d97706",
    ),
]
for index, (label, value, qualifier, color) in enumerate(chips):
    x = 50 + index * 290
    element("rect", x=x, y=108, width=265, height=72, rx=12, fill="#ffffff", stroke="#dde3ef")
    text(x + 18, 132, label, 13, 500, "#66738f")
    text(x + 18, 163, value, 25, 700, color)
    text(x + 120, 162, qualifier, 13, 500, "#66738f")

# Panel A: paired execution deltas by baseline latency.
panel_x, panel_y, panel_w, panel_h = 50, 210, 870, 470
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 36, "Effect by baseline block execution time", 19, 650, "#102044")
delta_subtitle = "Dowse cache vs no Dowse for each block; lower is better"
if len(deltas) >= 100:
    delta_subtitle += "; outer 1% clipped"
text(panel_x + 24, panel_y + 60, delta_subtitle, 13, 400, "#66738f")
plot_left, plot_top = panel_x + 64, panel_y + 92
plot_width, plot_height = panel_w - 94, panel_h - 180
if len(deltas) >= 100:
    central_low = percentile(deltas, 0.01)
    central_high = percentile(deltas, 0.99)
else:
    central_low = min(deltas)
    central_high = max(deltas)
y_min, y_max, y_ticks = nice_axis(min(-10, central_low), max(10, central_high), 9)
baseline_times_ms = [raw_time / 1000 for raw_time in raw_times]
x_min, x_max, x_ticks = nice_axis(min(baseline_times_ms), max(baseline_times_ms), 6)


def chart_y(value):
    return plot_top + (y_max - value) * plot_height / (y_max - y_min)


def chart_x(value):
    return plot_left + (value - x_min) * plot_width / (x_max - x_min)


for index, tick in enumerate(y_ticks):
    y = chart_y(tick)
    element("line", x1=plot_left, y1=y, x2=plot_left + plot_width, y2=y, stroke="#e7ebf3", stroke_width=1)
    if len(y_ticks) <= 7 or index % 2 == 0:
        text(plot_left - 12, y + 5, f"{tick:g}%", 12, 400, "#7c879e", "end")
for tick in x_ticks:
    x = chart_x(tick)
    element("line", x1=x, y1=plot_top, x2=x, y2=plot_top + plot_height, stroke="#f0f2f7", stroke_width=1)
    text(x, plot_top + plot_height + 25, f"{tick:g} ms", 11, 400, "#7c879e", "middle")
element("line", x1=plot_left, y1=chart_y(0), x2=plot_left + plot_width, y2=chart_y(0), stroke="#95a0b5", stroke_width=1.5)
element(
    "line",
    x1=plot_left,
    y1=chart_y(execution_delta),
    x2=plot_left + plot_width,
    y2=chart_y(execution_delta),
    stroke="#0052ff",
    stroke_width=2,
    stroke_dasharray="7 6",
)
for record, raw_time, delta in zip(records, raw_times, deltas):
    x = chart_x(raw_time / 1000)
    color = "#7556f3" if record["result"]["cachedFirst"] else "#00a37a"
    radius = 5 if len(records) < 100 else 1.5
    element(
        "circle",
        cx=x,
        cy=chart_y(max(y_min, min(y_max, delta))),
        r=radius,
        fill=color,
        opacity=1 if len(records) < 100 else 0.3,
    )
element("circle", cx=panel_x + 25, cy=panel_y + panel_h - 22, r=5, fill="#7556f3")
text(panel_x + 37, panel_y + panel_h - 17, "Dowse replay first", 12, 400, "#66738f")
element("circle", cx=panel_x + 172, cy=panel_y + panel_h - 22, r=5, fill="#00a37a")
text(panel_x + 184, panel_y + panel_h - 17, "No-Dowse replay first", 12, 400, "#66738f")
element(
    "line",
    x1=panel_x + 340,
    y1=panel_y + panel_h - 22,
    x2=panel_x + 368,
    y2=panel_y + panel_h - 22,
    stroke="#0052ff",
    stroke_width=2,
    stroke_dasharray="7 6",
)
text(panel_x + 378, panel_y + panel_h - 17, f"Order-balanced {execution_delta:.1f}%", 12, 500, "#66738f")

# Panel B: provider-read reduction.
panel_x, panel_y, panel_w, panel_h = 950, 210, 440, 470
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 36, "Parent-state reads", 19, 650, "#102044")
text(panel_x + 24, panel_y + 60, "Normalized to no Dowse = 100%", 13, 400, "#66738f")
base_y = panel_y + 390
bar_top = panel_y + 105
bar_height = base_y - bar_top
groups = (("Accounts", "account"), ("Storage", "storage"), ("Bytecode", "code"))
for index, (label, key) in enumerate(groups):
    center = panel_x + 80 + index * 135
    raw, cached, delta = provider[key]
    cached_ratio = cached / raw
    element("rect", x=center - 32, y=bar_top, width=27, height=bar_height, rx=4, fill="#cbd3e1")
    element(
        "rect",
        x=center + 5,
        y=base_y - bar_height * cached_ratio,
        width=27,
        height=bar_height * cached_ratio,
        rx=4,
        fill="#0052ff",
    )
    text(center, base_y + 27, label, 12, 600, "#4d5b78", "middle")
    text(center + 18, base_y - bar_height * cached_ratio - 10, f"{delta:.1f}%", 12, 650, "#0052ff", "middle")
text(panel_x + 24, panel_y + panel_h - 24, "■", 15, 600, "#cbd3e1")
text(panel_x + 42, panel_y + panel_h - 24, "No Dowse", 12, 400, "#66738f")
text(panel_x + 128, panel_y + panel_h - 24, "■", 15, 600, "#0052ff")
text(panel_x + 146, panel_y + panel_h - 24, "Dowse", 12, 400, "#66738f")

# Panel C: critical-path comparison.
panel_x, panel_y, panel_w, panel_h = 50, 710, 1340, 320
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 38, "Mean per-block critical-path time", 19, 650, "#102044")
text(panel_x + 24, panel_y + 62, "Background prefetch must finish before execution to realize the cache benefit", 13, 400, "#66738f")
values = [
    ("No Dowse", statistics.fmean(raw_times) / 1000, "#94a0b5"),
    ("Dowse · prefetch hidden", statistics.fmean(cached_times) / 1000, "#00a37a"),
    (
        "Dowse · planning + prefetch serial",
        (statistics.fmean(cached_times) + statistics.fmean(planning_times) + statistics.fmean(prefetch_times)) / 1000,
        "#d97706",
    ),
]
axis_left = panel_x + 315
axis_width = panel_w - 370
_, critical_max, critical_ticks = nice_axis(0, max(value for _, value, _ in values) * 1.1, 5)
for tick in critical_ticks:
    x = axis_left + tick / critical_max * axis_width
    element("line", x1=x, y1=panel_y + 90, x2=x, y2=panel_y + 267, stroke="#edf0f6", stroke_width=1)
    text(x, panel_y + 290, f"{tick:g} ms", 11, 400, "#7c879e", "middle")
for index, (label, value, color) in enumerate(values):
    y = panel_y + 103 + index * 57
    text(axis_left - 18, y + 22, label, 13, 550, "#4d5b78", "end")
    element("rect", x=axis_left, y=y, width=value / critical_max * axis_width, height=32, rx=5, fill=color)
    text(axis_left + value / critical_max * axis_width + 10, y + 22, f"{value:.1f} ms", 13, 650, color)

# Panel D: latency distribution.
panel_x, panel_y, panel_w, panel_h = 50, 1060, 1340, 275
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 38, "Per-block execution latency distribution", 19, 650, "#102044")
text(panel_x + 24, panel_y + 62, f"Descriptive percentiles from {len(records):,} historical blocks; not a production sequencer-tail estimate", 13, 400, "#66738f")
percentiles = (("p50", 0.50), ("p90", 0.90), ("p95", 0.95), ("p99", 0.99))
group_left = panel_x + 90
group_width = (panel_w - 150) / len(percentiles)
bar_base = panel_y + 225
percentile_values = [percentile(times, quantile) / 1000 for _, quantile in percentiles for times in (raw_times, cached_times)]
bar_scale = 135 / (max(percentile_values) * 1.1)
for index, (label, quantile) in enumerate(percentiles):
    center = group_left + index * group_width + group_width / 2
    raw_value = percentile(raw_times, quantile) / 1000
    cached_value = percentile(cached_times, quantile) / 1000
    raw_height = raw_value * bar_scale
    cached_height = cached_value * bar_scale
    element("rect", x=center - 42, y=bar_base - raw_height, width=32, height=raw_height, rx=4, fill="#cbd3e1")
    element("rect", x=center + 10, y=bar_base - cached_height, width=32, height=cached_height, rx=4, fill="#0052ff")
    text(center - 26, bar_base - raw_height - 8, f"{raw_value:.1f}", 11, 600, "#66738f", "middle")
    text(center + 26, bar_base - cached_height - 8, f"{cached_value:.1f}", 11, 650, "#0052ff", "middle")
    text(center, bar_base + 24, label, 12, 650, "#4d5b78", "middle")
text(panel_x + panel_w - 215, panel_y + 36, "■", 15, 600, "#cbd3e1")
text(panel_x + panel_w - 197, panel_y + 36, "No Dowse", 12, 400, "#66738f")
text(panel_x + panel_w - 112, panel_y + 36, "■", 15, 600, "#0052ff")
text(panel_x + panel_w - 94, panel_y + 36, "Dowse", 12, 400, "#66738f")

text(50, 1371, f"Historical replay, not a production sequencer A/B. Order alternated by block; the first execution measured {first_replay_multiplier:.2f}× the second.", 13, 500, "#66738f")
svg.append("</svg>")

OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
OUTPUT_PATH.write_text("\n".join(svg) + "\n")
print(OUTPUT_PATH)
