#!/usr/bin/env python3
"""Render independent full-range Dowse replay arms, optionally with a bracketing control."""

import json
import math
import statistics
import sys
from pathlib import Path
from xml.sax.saxutils import escape


RAW_PATH = Path(sys.argv[1])
DOWSE_PATH = Path(sys.argv[2])
OUTPUT_PATH = Path(sys.argv[3])
RAW_BRACKET_PATH = Path(sys.argv[4]) if len(sys.argv) > 4 else None
WIDTH = 1440
HEIGHT = 1400
FONT = "Inter, ui-sans-serif, system-ui, -apple-system, sans-serif"


def load_arm(path, expected_variants):
    with path.open() as source:
        lines = [json.loads(line) for line in source]
    metadata = lines[0]
    if metadata.get("variant") not in expected_variants:
        expected = " or ".join(expected_variants)
        raise ValueError(f"{path} is not the {expected} arm")
    return metadata, {line["block"]: line for line in lines[1:] if line.get("kind") == "block"}


raw_metadata, raw_by_block = load_arm(RAW_PATH, ("raw",))
dowse_metadata, dowse_by_block = load_arm(DOWSE_PATH, ("dowse", "concurrent"))
raw_bracket_metadata, raw_bracket_by_block = (
    load_arm(RAW_BRACKET_PATH, ("raw",)) if RAW_BRACKET_PATH else (None, None)
)
concurrent = dowse_metadata["variant"] == "concurrent"
treatment_label = "Concurrent Dowse" if concurrent else "Dowse"


def declared_blocks(metadata):
    if "blocks" in metadata:
        return metadata["blocks"]
    return list(range(metadata["startBlock"], metadata["endBlock"] + 1))


raw_declared_blocks = declared_blocks(raw_metadata)
if raw_declared_blocks != declared_blocks(dowse_metadata):
    raise ValueError("replay arms cover different block ranges")
if raw_by_block.keys() != dowse_by_block.keys():
    raise ValueError("replay arms contain different blocks")
if raw_bracket_metadata and (
    raw_declared_blocks != declared_blocks(raw_bracket_metadata)
    or raw_by_block.keys() != raw_bracket_by_block.keys()
):
    raise ValueError("bracketing no-Dowse arm covers a different block range")

blocks = sorted(raw_by_block)
for block in blocks:
    raw = raw_by_block[block]
    dowse = dowse_by_block[block]
    if (
        raw["replay"]["blockHash"] != dowse["replay"]["blockHash"]
        or raw["gasUsed"] != dowse["gasUsed"]
        or raw["transactionCount"] != dowse["transactionCount"]
    ):
        raise ValueError(f"replay arms disagree at block {block}")
    if raw_bracket_by_block:
        raw_bracket = raw_bracket_by_block[block]
        if (
            raw["replay"]["blockHash"] != raw_bracket["replay"]["blockHash"]
            or raw["gasUsed"] != raw_bracket["gasUsed"]
            or raw["transactionCount"] != raw_bracket["transactionCount"]
        ):
            raise ValueError(f"bracketing no-Dowse arm disagrees at block {block}")

raw_times = [
    (
        raw_by_block[block]["replay"]["executionTimeUs"]
        + raw_bracket_by_block[block]["replay"]["executionTimeUs"]
    )
    / 2
    if raw_bracket_by_block
    else raw_by_block[block]["replay"]["executionTimeUs"]
    for block in blocks
]
dowse_times = [dowse_by_block[block]["replay"]["executionTimeUs"] for block in blocks]


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
    ticks = [
        axis_low + index * step
        for index in range(round((axis_high - axis_low) / step) + 1)
    ]
    return axis_low, axis_high, ticks


def change(before, after):
    return (after / before - 1) * 100


aggregate_change = change(sum(raw_times), sum(dowse_times))
mean_raw = statistics.fmean(raw_times) / 1000
mean_dowse = statistics.fmean(dowse_times) / 1000
p99_raw = percentile(raw_times, 0.99) / 1000
p99_dowse = percentile(dowse_times, 0.99) / 1000

provider = {}
for kind in ("account", "storage", "code"):
    field = f"{kind}Fetches"
    raw = sum(
        (
            raw_by_block[block]["replay"]["stateProvider"][field]
            + raw_bracket_by_block[block]["replay"]["stateProvider"][field]
        )
        / 2
        if raw_bracket_by_block
        else raw_by_block[block]["replay"]["stateProvider"][field]
        for block in blocks
    )
    dowse = sum(dowse_by_block[block]["replay"]["stateProvider"][field] for block in blocks)
    provider[kind] = (raw, dowse, change(raw, dowse))

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


svg.append(
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" '
    f'viewBox="0 0 {WIDTH} {HEIGHT}">'
)
element("rect", x=0, y=0, width=WIDTH, height=HEIGHT, fill="#f5f7fb")
title = "Dowse concurrent state-prefetch replay" if concurrent else "Dowse independent-arm replay"
text(50, 56, title, 30, 700, "#102044")
text(
    50,
    84,
    f"{len(blocks):,} Base mainnet blocks · {sum(raw_by_block[block]['transactionCount'] for block in blocks):,} transactions · blocks {blocks[0]:,}–{blocks[-1]:,}"
    + (
        f" · {dowse_metadata['config']['workers']} workers, {dowse_metadata['config']['headStartUs'] / 1000:g} ms requested lead"
        if concurrent
        else ""
    ),
    15,
    400,
    "#66738f",
)

chips = [
    ("Cumulative execution", f"{aggregate_change:+.1f}%", "#0052ff"),
    ("Mean per block", f"{mean_raw:.1f} → {mean_dowse:.1f} ms", "#00a37a"),
    ("p99", f"{p99_raw:.1f} → {p99_dowse:.1f} ms", "#7556f3"),
    ("Storage reads", f"{provider['storage'][2]:+.1f}%", "#d97706"),
]
for index, (label, value, color) in enumerate(chips):
    x = 50 + index * 335
    element("rect", x=x, y=108, width=310, height=72, rx=12, fill="#ffffff", stroke="#dde3ef")
    text(x + 18, 132, label, 13, 500, "#66738f")
    text(x + 18, 163, value, 23, 700, color)

# Panel A: block-for-block latency comparison.
panel_x, panel_y, panel_w, panel_h = 50, 210, 870, 470
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 36, "Block-for-block execution latency", 19, 650, "#102044")
comparison_subtitle = (
    "No-Dowse is the mean of two bracketing arms; axes clipped at p99.5"
    if raw_bracket_by_block
    else "Each block executed once in each full-range arm; axes clipped at p99.5"
)
text(panel_x + 24, panel_y + 60, comparison_subtitle, 13, 400, "#66738f")
plot_left, plot_top = panel_x + 72, panel_y + 86
plot_width, plot_height = panel_w - 104, panel_h - 145
limit = max(percentile(raw_times, 0.995), percentile(dowse_times, 0.995)) / 1000
axis_low, axis_high, axis_ticks = nice_axis(0, limit, 6)


def scatter_x(value):
    return plot_left + (min(value, axis_high) - axis_low) * plot_width / (axis_high - axis_low)


def scatter_y(value):
    return plot_top + (axis_high - min(value, axis_high)) * plot_height / (axis_high - axis_low)


for tick in axis_ticks:
    x = scatter_x(tick)
    y = scatter_y(tick)
    element("line", x1=x, y1=plot_top, x2=x, y2=plot_top + plot_height, stroke="#edf0f6")
    element("line", x1=plot_left, y1=y, x2=plot_left + plot_width, y2=y, stroke="#edf0f6")
    text(x, plot_top + plot_height + 25, f"{tick:g}", 11, 400, "#7c879e", "middle")
    text(plot_left - 12, y + 4, f"{tick:g}", 11, 400, "#7c879e", "end")
element("line", x1=plot_left, y1=scatter_y(axis_low), x2=scatter_x(axis_high), y2=plot_top, stroke="#95a0b5", stroke_width=1.5)
for raw, dowse in zip(raw_times, dowse_times):
    element(
        "circle",
        cx=scatter_x(raw / 1000),
        cy=scatter_y(dowse / 1000),
        r=1.6,
        fill="#0052ff" if dowse < raw else "#d97706",
        opacity=0.34,
    )
text(plot_left + plot_width / 2, panel_y + panel_h - 14, "No Dowse execution (ms)", 12, 500, "#66738f", "middle")
text(plot_left, plot_top - 10, "Concurrent execution (ms)" if concurrent else "Dowse execution (ms)", 12, 500, "#66738f")
text(panel_x + panel_w - 204, panel_y + 36, "● faster", 12, 500, "#0052ff")
text(panel_x + panel_w - 122, panel_y + 36, "● slower", 12, 500, "#d97706")

# Panel B: provider-read reduction.
panel_x, panel_y, panel_w, panel_h = 950, 210, 440, 470
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 36, "Parent-state reads", 19, 650, "#102044")
text(panel_x + 24, panel_y + 60, "Normalized to no Dowse = 100%", 13, 400, "#66738f")
base_y = panel_y + 390
bar_top = panel_y + 105
bar_height = base_y - bar_top
for index, (label, key) in enumerate((("Accounts", "account"), ("Storage", "storage"), ("Bytecode", "code"))):
    center = panel_x + 80 + index * 135
    raw, dowse, delta = provider[key]
    dowse_ratio = dowse / raw
    element("rect", x=center - 32, y=bar_top, width=27, height=bar_height, rx=4, fill="#cbd3e1")
    element("rect", x=center + 5, y=base_y - bar_height * dowse_ratio, width=27, height=bar_height * dowse_ratio, rx=4, fill="#0052ff")
    text(center, base_y + 27, label, 12, 600, "#4d5b78", "middle")
    text(center + 18, base_y - bar_height * dowse_ratio - 10, f"{delta:.1f}%", 12, 650, "#0052ff", "middle")
text(panel_x + 24, panel_y + panel_h - 24, "■", 15, 600, "#cbd3e1")
text(panel_x + 42, panel_y + panel_h - 24, "No Dowse", 12, 400, "#66738f")
text(panel_x + 128, panel_y + panel_h - 24, "■", 15, 600, "#0052ff")
text(panel_x + 146, panel_y + panel_h - 24, treatment_label, 12, 400, "#66738f")

# Panel C: execution percentiles.
panel_x, panel_y, panel_w, panel_h = 50, 710, 650, 310
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 38, "Execution latency distribution", 19, 650, "#102044")
text(panel_x + panel_w - 192, panel_y + 38, "■ No Dowse", 11, 500, "#94a0b5")
text(panel_x + panel_w - 100, panel_y + 38, "■ Concurrent", 11, 500, "#0052ff")
distribution_subtitle = (
    f"{treatment_label} arm vs mean of two bracketing no-Dowse arms"
    if raw_bracket_by_block
    else "Independent full-range arms"
)
text(panel_x + 24, panel_y + 62, distribution_subtitle, 13, 400, "#66738f")
percentiles = (("p50", 0.50), ("p90", 0.90), ("p95", 0.95), ("p99", 0.99))
all_percentiles = [percentile(times, quantile) / 1000 for _, quantile in percentiles for times in (raw_times, dowse_times)]
bar_base = panel_y + 255
bar_scale = 145 / (max(all_percentiles) * 1.1)
group_width = 135
for index, (label, quantile) in enumerate(percentiles):
    center = panel_x + 95 + index * group_width
    raw = percentile(raw_times, quantile) / 1000
    dowse = percentile(dowse_times, quantile) / 1000
    raw_height = raw * bar_scale
    dowse_height = dowse * bar_scale
    element("rect", x=center - 34, y=bar_base - raw_height, width=28, height=raw_height, rx=4, fill="#cbd3e1")
    element("rect", x=center + 6, y=bar_base - dowse_height, width=28, height=dowse_height, rx=4, fill="#0052ff")
    text(center - 20, bar_base - raw_height - 7, f"{raw:.0f}", 10, 600, "#66738f", "middle")
    text(center + 20, bar_base - dowse_height - 7, f"{dowse:.0f}", 10, 650, "#0052ff", "middle")
    text(center, bar_base + 24, label, 12, 650, "#4d5b78", "middle")

# Panel D: effect by no-Dowse baseline quartile.
panel_x, panel_y, panel_w, panel_h = 730, 710, 660, 310
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 38, "Effect by initial execution time", 19, 650, "#102044")
quartile_subtitle = (
    "Quartiles defined by the bracketing no-Dowse mean"
    if raw_bracket_by_block
    else "Quartiles defined by the independent no-Dowse arm"
)
text(panel_x + 24, panel_y + 62, quartile_subtitle, 13, 400, "#66738f")
ordered = sorted(range(len(blocks)), key=raw_times.__getitem__)
quartiles = [ordered[index * len(blocks) // 4 : (index + 1) * len(blocks) // 4] for index in range(4)]
quartile_values = [
    change(sum(raw_times[index] for index in group), sum(dowse_times[index] for index in group))
    for group in quartiles
]
axis_left = panel_x + 130
axis_width = panel_w - 180
effect_min, effect_max, effect_ticks = nice_axis(min(-5, min(quartile_values)), max(5, max(quartile_values)), 5)
for tick in effect_ticks:
    x = axis_left + (tick - effect_min) / (effect_max - effect_min) * axis_width
    element("line", x1=x, y1=panel_y + 85, x2=x, y2=panel_y + 260, stroke="#edf0f6")
    text(x, panel_y + 283, f"{tick:g}%", 11, 400, "#7c879e", "middle")
zero_x = axis_left + (0 - effect_min) / (effect_max - effect_min) * axis_width
element("line", x1=zero_x, y1=panel_y + 85, x2=zero_x, y2=panel_y + 260, stroke="#95a0b5", stroke_width=1.5)
for index, value in enumerate(quartile_values):
    y = panel_y + 96 + index * 42
    text(axis_left - 14, y + 18, f"Q{index + 1}", 12, 600, "#4d5b78", "end")
    value_x = axis_left + (value - effect_min) / (effect_max - effect_min) * axis_width
    start_x = min(zero_x, value_x)
    element("rect", x=start_x, y=y, width=max(2, abs(value_x - zero_x)), height=25, rx=4, fill="#0052ff" if value < 0 else "#d97706")
    text(value_x + (-8 if value < 0 else 8), y + 18, f"{value:+.1f}%", 12, 650, "#0052ff" if value < 0 else "#d97706", "end" if value < 0 else "start")

# Panel E: rolling execution latency over the fixed range.
panel_x, panel_y, panel_w, panel_h = 50, 1050, 1340, 285
element("rect", x=panel_x, y=panel_y, width=panel_w, height=panel_h, rx=14, fill="#ffffff", stroke="#dde3ef")
text(panel_x + 24, panel_y + 38, "Execution latency through the range", 19, 650, "#102044")
text(panel_x + 24, panel_y + 62, "50-block rolling mean", 13, 400, "#66738f")
window = 50
raw_rolling = [statistics.fmean(raw_times[max(0, index - window + 1) : index + 1]) / 1000 for index in range(len(blocks))]
dowse_rolling = [statistics.fmean(dowse_times[max(0, index - window + 1) : index + 1]) / 1000 for index in range(len(blocks))]
plot_left, plot_top = panel_x + 70, panel_y + 84
plot_width, plot_height = panel_w - 105, panel_h - 135
rolling_max = max(percentile(raw_rolling, 0.995), percentile(dowse_rolling, 0.995))
rolling_low, rolling_high, rolling_ticks = nice_axis(0, rolling_max, 4)
text(plot_left, plot_top - 8, "Execution (ms)", 11, 500, "#66738f")
for tick in rolling_ticks:
    y = plot_top + (rolling_high - tick) / (rolling_high - rolling_low) * plot_height
    element("line", x1=plot_left, y1=y, x2=plot_left + plot_width, y2=y, stroke="#edf0f6")
    text(plot_left - 12, y + 4, f"{tick:g}", 11, 400, "#7c879e", "end")
for values, color in ((raw_rolling, "#94a0b5"), (dowse_rolling, "#0052ff")):
    points = []
    for index, value in enumerate(values):
        x = plot_left + index / max(1, len(values) - 1) * plot_width
        y = plot_top + (rolling_high - min(value, rolling_high)) / (rolling_high - rolling_low) * plot_height
        points.append(f"{x:.1f},{y:.1f}")
    element("polyline", points=" ".join(points), fill="none", stroke=color, stroke_width=2)
text(plot_left, plot_top + plot_height + 20, f"{blocks[0]:,}", 10, 400, "#7c879e")
text(
    plot_left + plot_width,
    plot_top + plot_height + 20,
    f"{blocks[-1]:,}",
    10,
    400,
    "#7c879e",
    "end",
)
text(
    plot_left + plot_width / 2,
    plot_top + plot_height + 20,
    "Block number",
    10,
    500,
    "#7c879e",
    "middle",
)
text(panel_x + panel_w - 205, panel_y + 38, "— No Dowse", 12, 600, "#94a0b5")
text(panel_x + panel_w - 108, panel_y + 38, f"— {treatment_label}", 12, 600, "#0052ff")
if concurrent:
    footer = "Historical replay; workers start after replay setup and race the EVM. Dowse is bracketed by restarted no-Dowse arms."
else:
    footer = (
        "Historical replay; Dowse is bracketed by two restarted no-Dowse arms. Prefetch planning and reads are outside measured execution."
        if raw_bracket_by_block
        else "Historical replay; each treatment ran as a separate full-range arm. Prefetch planning and reads are outside measured execution."
    )
text(50, 1373, footer, 13, 500, "#66738f")
svg.append("</svg>")

OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
OUTPUT_PATH.write_text("\n".join(svg) + "\n")
print(OUTPUT_PATH)
