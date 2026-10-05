#!/usr/bin/env python3
"""Judge calibration against human labels. Required before any scored run.

  python3 calibrate.py sample results/<run_id> --n 30
      Writes calibration/<run_id>.jsonl with the judge verdict hidden in a separate field.
      Fill each line's "human" with yes, no or unknown, using the same definitions as the judge.
      Include plans with injected flaws and plans that omit an item entirely.
  python3 calibrate.py score calibration/<run_id>.jsonl
      Per-item agreement over all three values (target at least 90%), plus how often the judge
      said unknown where the human said yes or no. A judge that falls back to unknown can look
      accurate while quietly failing plans. Re-calibrate when the judge model or rubric changes.
"""

import argparse
import collections
import json
import random
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TARGET_AGREEMENT = 0.90


def sample(run_dir, n, seed):
    rows = []
    for path in sorted(Path(run_dir).glob("*/*/trial-*.json")):
        if path.name.endswith(".transcript.json"):
            continue
        record = json.loads(path.read_text())
        if not record.get("judge"):
            continue
        transcript = json.loads((ROOT / record["transcript_path"]).read_text())
        case = json.loads((ROOT / "cases" / f"{record['case_id']}.json").read_text())
        checks = {item["id"]: item for item in case["rubric"]}
        for item_id, verdict in record["judge"]["items"].items():
            rows.append({"trial": str(path.relative_to(run_dir)), "case": record["case_id"], "item": item_id,
                         "critical": checks[item_id]["critical"], "check": checks[item_id]["check"],
                         "plan": transcript["final_text"], "judge": verdict["verdict"], "human": ""})
    random.Random(seed).shuffle(rows)
    out = ROOT / "calibration" / f"{Path(run_dir).name}.jsonl"
    out.parent.mkdir(exist_ok=True)
    out.write_text("".join(json.dumps(r) + "\n" for r in rows[:n]))
    print(f"wrote {min(n, len(rows))} items to {out}; label \"human\" without reading \"judge\"")


def score(path):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
    rows = [r for r in rows if r["human"] in ("yes", "no", "unknown") and r["judge"] in ("yes", "no", "unknown")]
    if not rows:
        raise SystemExit("no labelled rows")
    overall = sum(r["judge"] == r["human"] for r in rows) / len(rows)
    print(f"{len(rows)} labelled items, overall agreement {overall:.0%} (target {TARGET_AGREEMENT:.0%})")
    fallback = [r for r in rows if r["judge"] == "unknown" and r["human"] in ("yes", "no")]
    print(f"judge said unknown where the human decided: {len(fallback)}")
    by_item = collections.defaultdict(list)
    for r in rows:
        by_item[(r["case"], r["item"])].append(r["judge"] == r["human"])
    print("\nper item:")
    for (case, item), hits in sorted(by_item.items()):
        rate = sum(hits) / len(hits)
        mark = "" if rate >= TARGET_AGREEMENT else "  <- below target"
        print(f"  {case}/{item}: {sum(hits)}/{len(hits)}{mark}")
    for r in rows:
        if r["judge"] != r["human"]:
            print(f"  disagree: {r['case']}/{r['item']} judge={r['judge']} human={r['human']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("sample")
    s.add_argument("run_dir")
    s.add_argument("--n", type=int, default=30)
    s.add_argument("--seed", type=int, default=0)
    c = sub.add_parser("score")
    c.add_argument("path")
    args = parser.parse_args()
    sample(args.run_dir, args.n, args.seed) if args.command == "sample" else score(args.path)


if __name__ == "__main__":
    main()
