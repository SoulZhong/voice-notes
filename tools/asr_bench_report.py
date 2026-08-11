#!/usr/bin/env python3
"""Summarize asr_bench output: one metrics row per engine JSONL.

Reads <out_dir>/*.jsonl produced by `asr_bench` (fields: reference/hypothesis/
audio_ms/elapsed_ms/...), reuses asr_eval.evaluate for CER/MER so the metric
definition has a single source, and prints a markdown comparison table.

Rows with an empty reference are excluded from CER/MER (no cloud contrast or
cloud failure); their count is reported so silent coverage gaps stay visible.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from asr_eval import evaluate  # noqa: E402


def load_rows(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def engine_summary(rows: list[dict]) -> dict:
    scored = [r for r in rows if str(r.get("reference", "")).strip()]
    metrics = evaluate(scored) if scored else {"cer": None, "mer": None, "utterances": 0}
    audio_ms = sum(int(r.get("audio_ms", 0)) for r in rows)
    elapsed_ms = sum(int(r.get("elapsed_ms", 0)) for r in rows)
    return {
        "segments": len(rows),
        "scored": len(scored),
        "unscored": len(rows) - len(scored),
        "cer": metrics["cer"],
        "mer": metrics["mer"],
        "rtf": (elapsed_ms / audio_ms) if audio_ms else None,
    }


def fmt_pct(v: float | None) -> str:
    return "-" if v is None else f"{v * 100:.2f}%"


def fmt_rtf(v: float | None) -> str:
    return "-" if v is None else f"{v:.3f}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("out_dir", type=Path, help="asr_bench output directory")
    args = parser.parse_args()
    files = sorted(args.out_dir.glob("*.jsonl"))
    if not files:
        print(f"no *.jsonl under {args.out_dir}", file=sys.stderr)
        return 1
    print("| engine | segments | scored | CER | MER | RTF |")
    print("|---|---|---|---|---|---|")
    for f in files:
        s = engine_summary(load_rows(f))
        print(
            f"| {f.stem} | {s['segments']} | {s['scored']}"
            f" | {fmt_pct(s['cer'])} | {fmt_pct(s['mer'])} | {fmt_rtf(s['rtf'])} |"
        )
        if s["unscored"]:
            print(f"  (excluded {s['unscored']} segments without reference)", file=sys.stderr)
    print("\nCER/MER vs cloud contrast reference; edit `reference` fields to make a golden set.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
