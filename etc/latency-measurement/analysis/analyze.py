#!/usr/bin/env python3
"""Offline analysis for the P2P block-latency measurement.

Loads append-only per-region observer CSVs, computes two latency views, and
emits a self-contained interactive HTML report plus a static headline PNG.

Metrics:
  (a) ABSOLUTE latency per row:
        abs_ms = recv_wallclock_ns/1e6 - (produced_sec*1000 + produced_millis_part)
      Coarse pre-Holocene because produced time is whole-second. Negative
      values indicate clock skew and are reported, not dropped.
  (b) CROSS-OBSERVER RELATIVE SPREAD (the defensible metric):
        per block_hash, t0 = min(recv_wallclock_ns) across all regions,
        rel_ms = (recv_wallclock_ns - t0)/1e6

Usage:
    python analyze.py --input-glob 'data/*.csv' --out-dir out/
"""

from __future__ import annotations

import glob
import argparse
from pathlib import Path

import numpy as np
import pandas as pd
import plotly.graph_objects as go
from plotly.subplots import make_subplots

# Canonical region display order (fastest -> slowest expected).
REGION_ORDER = [
    "us-east",
    "us-west",
    "eu-central",
    "eu-north",
    "ap-northeast",
    "ap-southeast",
]

CSV_COLUMNS = [
    "recv_wallclock_ns",
    "block_number",
    "block_hash",
    "produced_sec",
    "produced_millis_part",
    "region",
    "peer_id",
]


class LatencyAnalysis:
    """Loads observer CSVs and computes absolute + cross-observer latency."""

    def __init__(self, df: pd.DataFrame):
        self.df = df

    @classmethod
    def from_glob(cls, input_glob: str) -> "LatencyAnalysis":
        paths = sorted(glob.glob(input_glob))
        if not paths:
            raise SystemExit(f"no CSV files matched glob: {input_glob!r}")
        frames = []
        for p in paths:
            # recv_wallclock_ns can exceed int64; read as object then coerce to a
            # Python-int-backed column so we never silently overflow.
            frame = pd.read_csv(p, dtype={"block_hash": str, "region": str, "peer_id": str})
            missing = set(CSV_COLUMNS) - set(frame.columns)
            if missing:
                raise SystemExit(f"{p}: missing columns {sorted(missing)}")
            frames.append(frame)
        df = pd.concat(frames, ignore_index=True)
        return cls(df)

    def compute(self) -> pd.DataFrame:
        # Reconstruct as an owned, non-view frame so assignments below never hit
        # pandas' chained-assignment path (CoW-transition FutureWarnings).
        df = self.df.reset_index(drop=True).copy()

        # recv_wallclock_ns is u128 in principle; pandas may read as int64/float.
        # Use float64 for the millisecond math (ms values stay well within f64
        # precision for realistic epoch timestamps), and keep the raw column.
        recv_ms = df["recv_wallclock_ns"].astype("float64") / 1e6
        produced_ms = (
            df["produced_sec"].astype("float64") * 1000.0
            + df["produced_millis_part"].astype("float64")
        )

        # (b) cross-observer relative spread, grouped by block_hash
        t0 = df.groupby("block_hash")["recv_wallclock_ns"].transform("min")
        rel_ms = (df["recv_wallclock_ns"].astype("float64") - t0.astype("float64")) / 1e6

        # (a) absolute latency; keep raw negatives, add a separately clamped column.
        abs_ms = recv_ms - produced_ms

        return df.assign(
            abs_ms=abs_ms,
            abs_ms_clamped=abs_ms.clip(lower=0.0),
            rel_ms=rel_ms,
            # Ordered categorical region for stable grouping/plotting.
            region=pd.Categorical(df["region"], categories=REGION_ORDER, ordered=True),
        )

    @staticmethod
    def _pctile(s: pd.Series, q: float) -> float:
        if s.empty:
            return float("nan")
        return float(np.percentile(s.to_numpy(), q))

    def absolute_summary(self, df: pd.DataFrame) -> pd.DataFrame:
        rows = []
        for region in REGION_ORDER:
            sub = df[df["region"] == region]["abs_ms"].dropna()
            neg = df[df["region"] == region]["abs_ms"]
            neg_count = int((neg < 0).sum())
            rows.append(
                {
                    "region": region,
                    "count": int(sub.shape[0]),
                    "abs_p50_ms": self._pctile(sub, 50),
                    "abs_p90_ms": self._pctile(sub, 90),
                    "abs_p99_ms": self._pctile(sub, 99),
                    "abs_max_ms": float(sub.max()) if not sub.empty else float("nan"),
                    "negative_count": neg_count,
                }
            )
        return pd.DataFrame(rows)

    def relative_summary(self, df: pd.DataFrame) -> pd.DataFrame:
        # Fastest-observer share: how often each region holds the per-hash min.
        # rel_ms == 0 marks the fastest observer for that block.
        winners = df[df["rel_ms"] == 0.0]
        win_counts = winners["region"].value_counts()
        total_wins = int(win_counts.sum())

        rows = []
        for region in REGION_ORDER:
            sub = df[df["region"] == region]["rel_ms"].dropna()
            wins = int(win_counts.get(region, 0))
            rows.append(
                {
                    "region": region,
                    "count": int(sub.shape[0]),
                    "rel_p50_ms": self._pctile(sub, 50),
                    "rel_p90_ms": self._pctile(sub, 90),
                    "rel_p99_ms": self._pctile(sub, 99),
                    "fastest_share": (wins / total_wins) if total_wins else float("nan"),
                }
            )
        return pd.DataFrame(rows)


class ReportBuilder:
    """Builds the interactive HTML report, headline PNG, and stdout table."""

    def __init__(
        self,
        df: pd.DataFrame,
        abs_summary: pd.DataFrame,
        rel_summary: pd.DataFrame,
    ):
        self.df = df
        self.abs_summary = abs_summary
        self.rel_summary = rel_summary

    def _headline_figure(self) -> go.Figure:
        fig = go.Figure()
        fig.add_bar(
            name="P50 abs_ms",
            x=self.abs_summary["region"],
            y=self.abs_summary["abs_p50_ms"],
        )
        fig.add_bar(
            name="P99 abs_ms",
            x=self.abs_summary["region"],
            y=self.abs_summary["abs_p99_ms"],
        )
        fig.update_layout(
            title="Absolute gossip latency by region (P50 vs P99)",
            barmode="group",
            xaxis_title="region",
            yaxis_title="latency (ms)",
            xaxis={"categoryorder": "array", "categoryarray": REGION_ORDER},
        )
        return fig

    def _rel_box_figure(self) -> go.Figure:
        fig = go.Figure()
        for region in REGION_ORDER:
            sub = self.df[self.df["region"] == region]["rel_ms"].dropna()
            fig.add_trace(go.Box(y=sub, name=region, boxpoints=False))
        fig.update_layout(
            title="Cross-observer relative spread rel_ms by region",
            xaxis_title="region",
            yaxis_title="rel_ms (ms behind fastest observer)",
        )
        return fig

    def _cdf_figure(self) -> go.Figure:
        fig = go.Figure()
        for region in REGION_ORDER:
            sub = self.df[self.df["region"] == region]["abs_ms"].dropna().sort_values()
            if sub.empty:
                continue
            y = np.arange(1, len(sub) + 1) / len(sub)
            fig.add_trace(go.Scatter(x=sub, y=y, mode="lines", name=region))
        fig.update_layout(
            title="Per-region CDF of absolute latency abs_ms",
            xaxis_title="abs_ms (ms)",
            yaxis_title="cumulative fraction",
        )
        return fig

    def _summary_table_figure(self) -> go.Figure:
        merged = self.abs_summary.merge(self.rel_summary, on="region", suffixes=("", "_rel"))
        display_cols = [
            "region",
            "count",
            "abs_p50_ms",
            "abs_p90_ms",
            "abs_p99_ms",
            "abs_max_ms",
            "negative_count",
            "rel_p50_ms",
            "rel_p90_ms",
            "rel_p99_ms",
            "fastest_share",
        ]
        cells = []
        for c in display_cols:
            col = merged[c]
            if col.dtype.kind == "f":
                cells.append([f"{v:.2f}" if pd.notna(v) else "" for v in col])
            else:
                cells.append(list(col.astype(str)))
        fig = go.Figure(
            data=[
                go.Table(
                    header={"values": display_cols, "align": "left"},
                    cells={"values": cells, "align": "left"},
                )
            ]
        )
        fig.update_layout(title="Summary table (absolute + cross-observer)")
        return fig

    def write_html(self, path: Path) -> None:
        fig = make_subplots(
            rows=4,
            cols=1,
            row_heights=[0.22, 0.24, 0.24, 0.30],
            specs=[[{"type": "xy"}], [{"type": "xy"}], [{"type": "xy"}], [{"type": "table"}]],
            subplot_titles=(
                "Absolute latency by region (P50 vs P99)",
                "Cross-observer relative spread rel_ms by region",
                "Per-region CDF of abs_ms",
                "Summary table",
            ),
            vertical_spacing=0.06,
        )

        for tr in self._headline_figure().data:
            fig.add_trace(tr, row=1, col=1)
        fig.update_layout(barmode="group")

        for tr in self._rel_box_figure().data:
            fig.add_trace(tr, row=2, col=1)

        for tr in self._cdf_figure().data:
            fig.add_trace(tr, row=3, col=1)

        for tr in self._summary_table_figure().data:
            fig.add_trace(tr, row=4, col=1)

        fig.update_layout(
            height=1600,
            title_text="P2P Block-Latency Measurement — Offline Analysis",
            showlegend=True,
        )
        fig.update_xaxes(categoryorder="array", categoryarray=REGION_ORDER, row=1, col=1)
        fig.update_xaxes(title_text="abs_ms (ms)", row=3, col=1)
        fig.update_yaxes(title_text="latency (ms)", row=1, col=1)
        fig.update_yaxes(title_text="rel_ms (ms)", row=2, col=1)
        fig.update_yaxes(title_text="cumulative fraction", row=3, col=1)

        fig.write_html(str(path), include_plotlyjs="inline", full_html=True)

    def write_png(self, path: Path) -> str:
        """Write the headline static PNG. Returns a status message."""
        fig = self._headline_figure()
        try:
            fig.write_image(str(path))  # needs kaleido
            return f"wrote {path}"
        except Exception as e:  # kaleido/engine missing or failed
            return (
                f"skipped static PNG ({path}): static-image engine unavailable "
                f"({type(e).__name__}: {e}). Install kaleido (pip install kaleido) to enable."
            )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-glob", required=True, help="glob for input CSVs, e.g. 'data/*.csv'")
    parser.add_argument("--out-dir", required=True, help="output directory")
    args = parser.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    analysis = LatencyAnalysis.from_glob(args.input_glob)
    df = analysis.compute()
    abs_summary = analysis.absolute_summary(df)
    rel_summary = analysis.relative_summary(df)

    # merged.csv: all rows + computed columns
    merged_path = out_dir / "merged.csv"
    df.to_csv(merged_path, index=False)

    # Interactive HTML + static PNG
    report = ReportBuilder(df, abs_summary, rel_summary)
    html_path = out_dir / "report.html"
    report.write_html(html_path)
    png_path = out_dir / "latency_by_region.png"
    png_status = report.write_png(png_path)

    # stdout summary
    combined = abs_summary.merge(rel_summary, on="region", suffixes=("", "_rel"))
    total_neg = int(abs_summary["negative_count"].sum())
    with pd.option_context("display.max_columns", None, "display.width", 200, "display.float_format", "{:.2f}".format):
        print("\n=== Summary (absolute + cross-observer relative) ===")
        print(combined.to_string(index=False))
    print(f"\nrows analyzed: {len(df)}   unique blocks: {df['block_hash'].nunique()}")
    if total_neg:
        print(f"WARNING: {total_neg} rows have negative abs_ms (NTP/clock skew); raw values kept, see abs_ms_clamped.")
    print(f"\nwrote {merged_path}")
    print(f"wrote {html_path}")
    print(png_status)


if __name__ == "__main__":
    main()
