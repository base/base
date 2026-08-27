#!/usr/bin/env python3
"""Render strict holdout coverage and balanced runtime results for learned Dowse hints."""

import json
import math
import sys
from pathlib import Path
from xml.sax.saxutils import escape


COVERAGE_PATH = Path(sys.argv[1])
RUNTIME_PATH = Path(sys.argv[2])
OUTPUT_PATH = Path(sys.argv[3])
WIDTH = 1320
HEIGHT = 1080
FONT = "Inter, ui-sans-serif, system-ui, -apple-system, sans-serif"
STATIC = "#94a0b5"
ADAPTIVE = "#7556f3"
GOOD = "#00a37a"
BAD = "#d97706"

coverage = json.loads(COVERAGE_PATH.read_text())
runtime = json.loads(RUNTIME_PATH.read_text())
static_coverage = coverage["static"]["total"]
adaptive_coverage = coverage["adaptive1000"]["total"]
comparison = runtime["comparison"]

svg = []


def attr(value):
    return escape(str(value), {'"': "&quot;"})


def element(name, **attributes):
    formatted = " ".join(
        f'{key.replace("_", "-")}="{attr(value)}"' for key, value in attributes.items()
    )
    svg.append(f"<{name} {formatted}/>")


def text(x, y, value, size=16, weight=400, fill="#17223b", anchor="start"):
    svg.append(
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(str(value))}</text>'
    )


def panel(x, y, width, height, title, subtitle):
    element(
        "rect",
        x=x,
        y=y,
        width=width,
        height=height,
        rx=14,
        fill="#ffffff",
        stroke="#dde3ef",
    )
    text(x + 24, y + 38, title, 19, 650, "#102044")
    text(x + 24, y + 62, subtitle, 13, 400, "#66738f")


svg.append(
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" '
    f'viewBox="0 0 {WIDTH} {HEIGHT}">'
)
element("rect", x=0, y=0, width=WIDTH, height=HEIGHT, fill="#f5f7fb")
text(50, 56, "Online hint learning: better predictions, worse zero-lead execution", 29, 700, "#102044")
text(
    50,
    84,
    "Strict chronological Base mainnet holdout · static 500-block table vs cumulative 1,000-block table",
    15,
    400,
    "#66738f",
)

chips = [
    (
        "Storage-target recall",
        f"{static_coverage['recall'] * 100:.2f}% → {adaptive_coverage['recall'] * 100:.2f}%",
        GOOD,
    ),
    (
        "Prediction precision",
        f"{static_coverage['precision'] * 100:.2f}% → {adaptive_coverage['precision'] * 100:.2f}%",
        GOOD,
    ),
    ("Useful targets found", f"+{adaptive_coverage['hits'] - static_coverage['hits']:,}", GOOD),
    ("Builder execution", f"{comparison['changePct']:+.2f}%", BAD),
]
for index, (label, value, color) in enumerate(chips):
    x = 50 + index * 310
    element("rect", x=x, y=108, width=285, height=72, rx=12, fill="#ffffff", stroke="#dde3ef")
    text(x + 18, 132, label, 13, 500, "#66738f")
    text(x + 18, 163, value, 22, 700, color)

# Coverage panel.
panel(
    50,
    210,
    590,
    350,
    "Strict next-500-block coverage",
    "Predictions scored only after both training windows",
)
plot_left, plot_top, plot_height = 120, 300, 205
for tick in range(0, 101, 20):
    y = plot_top + plot_height * (1 - tick / 100)
    element("line", x1=plot_left, y1=y, x2=610, y2=y, stroke="#edf0f6")
    text(plot_left - 12, y + 4, f"{tick}%", 11, 400, "#7c879e", "end")
for index, (label, key) in enumerate((("Recall", "recall"), ("Precision", "precision"))):
    center = 255 + index * 230
    values = (static_coverage[key] * 100, adaptive_coverage[key] * 100)
    for offset, value, color in ((-50, values[0], STATIC), (14, values[1], ADAPTIVE)):
        height = value / 100 * plot_height
        element("rect", x=center + offset, y=plot_top + plot_height - height, width=36, height=height, rx=4, fill=color)
        text(center + offset + 18, plot_top + plot_height - height - 8, f"{value:.2f}%", 11, 650, color, "middle")
    text(center, plot_top + plot_height + 27, label, 13, 650, "#4d5b78", "middle")
text(368, 542, "■ Static 500 blocks", 12, 500, STATIC, "end")
text(590, 542, "■ Cumulative 1,000 blocks", 12, 500, ADAPTIVE, "end")

# Position-balanced runtime panel.
panel(
    670,
    210,
    600,
    350,
    "Zero-lead execution by run position",
    "A-B-B-A plus B-A-A-B; each bar is the complete 500-block arm",
)
positions = [runtime["positionBalanced"][str(index)] for index in range(1, 5)]
runtime_max = math.ceil(max(max(item["staticMs"], item["adaptiveMs"]) for item in positions) / 10_000) * 10
plot_left, plot_top, plot_height = 735, 300, 205
for tick in range(0, runtime_max + 1, 10):
    y = plot_top + plot_height * (1 - tick / runtime_max)
    element("line", x1=plot_left, y1=y, x2=1240, y2=y, stroke="#edf0f6")
    text(plot_left - 12, y + 4, f"{tick}s", 11, 400, "#7c879e", "end")
for index, item in enumerate(positions):
    center = 815 + index * 120
    for offset, key, color in ((-28, "staticMs", STATIC), (4, "adaptiveMs", ADAPTIVE)):
        value = item[key] / 1000
        height = value / runtime_max * plot_height
        element("rect", x=center + offset, y=plot_top + plot_height - height, width=26, height=height, rx=4, fill=color)
    text(center, plot_top + plot_height + 24, f"Position {index + 1}", 11, 600, "#4d5b78", "middle")
    text(center, plot_top + plot_height + 43, f"{item['changePct']:+.1f}%", 11, 650, GOOD if item["changePct"] < 0 else BAD, "middle")

# Runtime effects panel.
panel(
    50,
    590,
    590,
    360,
    "Why the learned table regressed",
    "Change from the static table; lower is better",
)
effects = [
    ("EVM storage reads", comparison["provider"]["storageFetches"]["changePct"]),
    ("EVM storage fetch time", comparison["provider"]["storageFetchTimeUs"]["changePct"]),
    ("Cumulative execution", comparison["changePct"]),
    ("p99 execution", comparison["percentiles"]["p99"]["changePct"]),
]
axis_min, axis_max = -2, 12
axis_left, axis_width = 280, 310
for tick in range(axis_min, axis_max + 1, 2):
    x = axis_left + (tick - axis_min) / (axis_max - axis_min) * axis_width
    element("line", x1=x, y1=690, x2=x, y2=895, stroke="#edf0f6")
    text(x, 920, f"{tick}%", 10, 400, "#7c879e", "middle")
zero_x = axis_left + (0 - axis_min) / (axis_max - axis_min) * axis_width
element("line", x1=zero_x, y1=690, x2=zero_x, y2=895, stroke="#95a0b5", stroke_width=1.5)
for index, (label, value) in enumerate(effects):
    y = 705 + index * 49
    text(axis_left - 14, y + 18, label, 12, 550, "#4d5b78", "end")
    value_x = axis_left + (value - axis_min) / (axis_max - axis_min) * axis_width
    start_x = min(zero_x, value_x)
    element("rect", x=start_x, y=y, width=max(2, abs(value_x - zero_x)), height=25, rx=4, fill=GOOD if value < 0 else BAD)
    label_x = zero_x + 8 if value < 0 else value_x + 8
    text(label_x, y + 18, f"{value:+.2f}%", 12, 650, GOOD if value < 0 else BAD)

# Chronological recall-delta panel.
panel(
    670,
    590,
    600,
    360,
    "Recall change through the holdout",
    "Cumulative table minus static table, in 50-block windows",
)
static_chunks = coverage["static"]["chunks"]
adaptive_chunks = coverage["adaptive1000"]["chunks"]
deltas = [
    (adaptive["recall"] - static["recall"]) * 100
    for static, adaptive in zip(static_chunks, adaptive_chunks)
]
delta_min = math.floor(min(-0.5, min(deltas)))
delta_max = math.ceil(max(2.0, max(deltas)))
plot_left, plot_top, plot_width, plot_height = 730, 690, 505, 205
zero_y = plot_top + delta_max / (delta_max - delta_min) * plot_height
for tick in range(delta_min, delta_max + 1):
    y = plot_top + (delta_max - tick) / (delta_max - delta_min) * plot_height
    element("line", x1=plot_left, y1=y, x2=plot_left + plot_width, y2=y, stroke="#edf0f6")
    text(plot_left - 12, y + 4, f"{tick:+d}pp", 10, 400, "#7c879e", "end")
for index, (chunk, value) in enumerate(zip(static_chunks, deltas)):
    center = plot_left + (index + 0.5) * plot_width / len(deltas)
    value_y = plot_top + (delta_max - value) / (delta_max - delta_min) * plot_height
    element("rect", x=center - 15, y=min(zero_y, value_y), width=30, height=max(2, abs(value_y - zero_y)), rx=3, fill=GOOD if value >= 0 else BAD)
    text(center, 918, str(chunk["blocks"][0])[-3:], 9, 400, "#7c879e", "middle")
text(plot_left + plot_width / 2, 936, "Block-number suffix", 10, 500, "#7c879e", "middle")

text(
    50,
    1000,
    "Coverage: frozen hints evaluated on blocks 50,493,200–50,493,699. Runtime: eight restarted whole-range arms, four per table, 4 workers, zero artificial lead.",
    12,
    400,
    "#66738f",
)
text(
    50,
    1024,
    "Conclusion: retain learning as an offline candidate generator; do not race the broader learned table when the builder has no lead-time budget.",
    12,
    600,
    "#102044",
)
svg.append("</svg>")
OUTPUT_PATH.write_text("\n".join(svg) + "\n")
