#!/usr/bin/env python3
"""Evaluate ASR JSONL without third-party dependencies.

Each line must contain `reference` and `hypothesis`. Optional fields:
`entities` (strings expected verbatim in hypothesis), `suppressed` (bool),
and `should_suppress` (bool).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def normalize(text: str) -> str:
    return "".join(ch.lower() for ch in text if ch.isalnum())


def tokenize(text: str) -> list[str]:
    """Mixed-language tokens: ASCII alnum runs become one word, other alnum
    chars (CJK etc.) stand alone. Gives CER semantics for Chinese and WER
    semantics for embedded English within a single metric (MER)."""
    tokens: list[str] = []
    word = ""
    for ch in text.lower():
        if ch.isascii() and ch.isalnum():
            word += ch
            continue
        if word:
            tokens.append(word)
            word = ""
        if ch.isalnum():
            tokens.append(ch)
    if word:
        tokens.append(word)
    return tokens


def edit_distance(left: str, right: str) -> int:
    previous = list(range(len(right) + 1))
    for i, a in enumerate(left, 1):
        current = [i]
        for j, b in enumerate(right, 1):
            current.append(min(current[-1] + 1, previous[j] + 1, previous[j - 1] + (a != b)))
        previous = current
    return previous[-1]


def evaluate(rows: list[dict]) -> dict:
    edits = ref_chars = entity_hits = entity_total = 0
    token_edits = ref_tokens = 0
    false_deletes = should_keep = 0
    for row in rows:
        reference = normalize(str(row["reference"]))
        hypothesis = normalize(str(row["hypothesis"]))
        edits += edit_distance(reference, hypothesis)
        ref_chars += len(reference)
        ref_tok = tokenize(str(row["reference"]))
        token_edits += edit_distance(ref_tok, tokenize(str(row["hypothesis"])))
        ref_tokens += len(ref_tok)
        for entity in row.get("entities", []):
            entity_total += 1
            entity_hits += normalize(str(entity)) in hypothesis
        if not row.get("should_suppress", False):
            should_keep += 1
            false_deletes += bool(row.get("suppressed", False))
    return {
        "utterances": len(rows),
        "cer": edits / ref_chars if ref_chars else 0.0,
        "mer": token_edits / ref_tokens if ref_tokens else 0.0,
        "entity_recall": entity_hits / entity_total if entity_total else None,
        "filter_false_delete_rate": false_deletes / should_keep if should_keep else None,
        "counts": {
            "edits": edits,
            "reference_chars": ref_chars,
            "token_edits": token_edits,
            "reference_tokens": ref_tokens,
            "entity_hits": entity_hits,
            "entity_total": entity_total,
            "false_deletes": false_deletes,
            "should_keep": should_keep,
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("dataset", type=Path, help="UTF-8 JSONL evaluation set")
    parser.add_argument("--max-cer", type=float)
    parser.add_argument("--max-mer", type=float)
    parser.add_argument("--min-entity-recall", type=float)
    parser.add_argument("--max-filter-fdr", type=float)
    args = parser.parse_args()
    rows = [json.loads(line) for line in args.dataset.read_text().splitlines() if line.strip()]
    metrics = evaluate(rows)
    print(json.dumps(metrics, ensure_ascii=False, indent=2))
    failed = (
        (args.max_cer is not None and metrics["cer"] > args.max_cer)
        or (args.max_mer is not None and metrics["mer"] > args.max_mer)
        or (args.min_entity_recall is not None and metrics["entity_recall"] is not None
            and metrics["entity_recall"] < args.min_entity_recall)
        or (args.max_filter_fdr is not None and metrics["filter_false_delete_rate"] is not None
            and metrics["filter_false_delete_rate"] > args.max_filter_fdr)
    )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
