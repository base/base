#!/usr/bin/env python3
"""Render the Dowse hint-coverage and provider-contention findings."""

import json
import sys
from pathlib import Path
from xml.sax.saxutils import escape


DATA_PATH = Path(sys.argv[1])
OUTPUT_PATH = Path(sys.argv[2])
WIDTH = 1440
HEIGHT = 1180
FONT = "Inter, ui-sans-serif, system-ui, -apple-system, sans-serif"

data = json.loads(DATA_PATH.read_text())
current = data["currentTable"]
recent = data["recentTable"]
residual = data["selectedResidualBlocks"]
effect = data["fullAbbaReplay"]["recentVsCurrent"]

svg = []


def element(tag, **attrs):
    attributes = " ".join(f'{name.replace("_", "-")}="{value}"' for name, value in attrs.items())
    svg.append(f"<{tag} {attributes}/>")


def text(x, y, value, size=16, weight=500, color="#d9e3f0", anchor="start"):
    svg.append(
        f'<text x="{x}" y="{y}" fill="{color}" font-family="{FONT}" '
        f'font-size="{size}" font-weight="{weight}" text-anchor="{anchor}">{escape(str(value))}</text>'
    )


def panel(x, y, width, height):
    element("rect", x=x, y=y, width=width, height=height, rx=18, fill="#101f34", stroke="#243956")


def metric_card(x, title, value, color):
    panel(x, 132, 310, 116)
    text(x + 22, 166, title.upper(), 12, 700, "#8093ad")
    text(x + 22, 218, value, 32, 750, color)


svg.append(
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" '
    f'viewBox="0 0 {WIDTH} {HEIGHT}">'
)
element("rect", x=0, y=0, width=WIDTH, height=HEIGHT, fill="#081426")
text(60, 66, "More hints remove reads — and still make the builder slower", 34, 760, "#f2f6fb")
text(
    60,
    101,
    "Strict forward holdout · Base blocks 50,492,700–50,494,699 · whole-range A-B-B-A arms",
    16,
    500,
    "#91a4bd",
)

metric_card(60, "EVM storage reads", f'{abs(effect["storageFetchesPct"]):.1f}% fewer', "#41d6a3")
metric_card(390, "EVM execution", f'{effect["executionPct"]:.1f}% slower', "#ff7a90")
metric_card(720, "Storage fetch time", f'{effect["storageFetchTimePct"]:.1f}% higher', "#ffb454")
metric_card(1050, "Worker-late residual", "0.5%", "#72a7ff")

panel(60, 278, 630, 330)
text(88, 319, "Static hint coverage", 22, 700, "#f2f6fb")
text(88, 345, "Recent 500-block table versus current 120-block table", 14, 500, "#91a4bd")
coverage_rows = [
    ("Destinations", current["addresses"], recent["addresses"]),
    ("Selectors", current["selectors"], recent["selectors"]),
    ("Hinted targets", current["items"], recent["items"]),
]
for index, (label, old, new) in enumerate(coverage_rows):
    y = 400 + index * 68
    text(88, y, label, 14, 600, "#b7c5d8")
    maximum = max(old, new)
    element("rect", x=220, y=y - 17, width=350, height=13, rx=6, fill="#1b304b")
    element("rect", x=220, y=y - 17, width=350 * old / maximum, height=13, rx=6, fill="#64748b")
    element("rect", x=220, y=y + 5, width=350 * new / maximum, height=13, rx=6, fill="#3b82f6")
    text(584, y - 5, f"{old:,}", 12, 650, "#a8b5c7")
    text(584, y + 18, f"{new:,}", 12, 650, "#72a7ff")
text(88, 574, "Current", 12, 650, "#a8b5c7")
element("rect", x=146, y=562, width=24, height=8, rx=4, fill="#64748b")
text(202, 574, "Recent", 12, 650, "#72a7ff")
element("rect", x=256, y=562, width=24, height=8, rx=4, fill="#3b82f6")

panel(720, 278, 660, 330)
text(748, 319, "Where residual storage latency comes from", 22, 700, "#f2f6fb")
text(748, 345, "Seven deliberately storage-heavy blocks · 1.00 s cumulative", 14, 500, "#91a4bd")
category_colors = ["#3b82f6", "#8b5cf6", "#d97706", "#64748b", "#2dd4bf"]
total_time = sum(category["timeUs"] for category in residual["categories"])
for index, (category, color) in enumerate(zip(residual["categories"], category_colors)):
    y = 391 + index * 42
    share = category["timeUs"] / total_time
    element("rect", x=748, y=y - 14, width=250, height=16, rx=6, fill="#1b304b")
    element("rect", x=748, y=y - 14, width=250 * share, height=16, rx=6, fill=color)
    text(1012, y, f"{share * 100:.1f}%", 13, 700, color)
    text(1068, y, category["name"], 13, 550, "#c5d0df")

panel(60, 638, 1320, 250)
text(88, 680, "The extra reads create contention instead of speed", 22, 700, "#f2f6fb")
text(88, 706, "Recent table relative to the current table; lower is better for every metric", 14, 500, "#91a4bd")
contention = [
    ("Storage reads", 100 + effect["storageFetchesPct"], "#41d6a3"),
    ("Storage fetch time", 100 + effect["storageFetchTimePct"], "#ffb454"),
    ("Account fetch time", 100 + effect["accountFetchTimePct"], "#ff7a90"),
    ("EVM execution", 100 + effect["executionPct"], "#ef476f"),
]
for index, (label, value, color) in enumerate(contention):
    x = 100 + index * 315
    text(x + 115, 746, label, 14, 650, "#c5d0df", "middle")
    element("line", x1=x, y1=840, x2=x + 230, y2=840, stroke="#30445e", stroke_width=2)
    element("rect", x=x + 42, y=840 - 68, width=54, height=68, rx=7, fill="#64748b")
    height = 68 * value / 100
    element("rect", x=x + 134, y=840 - height, width=54, height=height, rx=7, fill=color)
    text(x + 69, 866, "100", 12, 650, "#a8b5c7", "middle")
    text(x + 161, 866, f"{value:.1f}", 12, 700, color, "middle")

panel(60, 918, 1320, 188)
text(88, 959, "What remains payload-derivable?", 22, 700, "#f2f6fb")
derivable = residual["payloadDerivability"]
total_fetches = sum(item["fetches"] for item in derivable)
x = 88
colors = ["#334155", "#3b82f6", "#8b5cf6", "#d97706", "#2dd4bf"]
for item, color in zip(derivable, colors):
    width = 1240 * item["fetches"] / total_fetches
    element("rect", x=x, y=986, width=width, height=28, fill=color)
    x += width
for index, (item, color) in enumerate(zip(derivable, colors)):
    column = index % 3
    row = index // 3
    x = 90 + column * 420
    y = 1048 + row * 27
    element("rect", x=x, y=y - 11, width=12, height=12, rx=3, fill=color)
    share = item["fetches"] / total_fetches * 100
    text(x + 20, y, f'{share:.1f}% {item["name"]}', 12, 550, "#b7c5d8")

text(
    720,
    1145,
    "Decision: keep the current 4-worker table for zero-lead racing; admit broader hints only with real lead or I/O pacing.",
    14,
    650,
    "#d9e3f0",
    "middle",
)
svg.append("</svg>")
OUTPUT_PATH.write_text("\n".join(svg) + "\n")
