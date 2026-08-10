"""asr_bench_report 纯逻辑测试:engine_summary 的过滤/聚合口径。"""

from asr_bench_report import engine_summary


def test_summary_excludes_rows_without_reference_and_aggregates_rtf():
    rows = [
        {"reference": "你好世界", "hypothesis": "你好世界", "audio_ms": 1000, "elapsed_ms": 100},
        {"reference": "", "hypothesis": "无对照的段", "audio_ms": 1000, "elapsed_ms": 300},
    ]
    s = engine_summary(rows)
    assert s["segments"] == 2
    assert s["scored"] == 1
    assert s["unscored"] == 1
    assert s["cer"] == 0.0
    # RTF 覆盖全部段(含无对照段):速度指标与对照可得性无关。
    assert s["rtf"] == (100 + 300) / 2000


def test_summary_all_unscored_reports_none_metrics():
    rows = [{"reference": " ", "hypothesis": "x", "audio_ms": 0, "elapsed_ms": 0}]
    s = engine_summary(rows)
    assert s["scored"] == 0
    assert s["cer"] is None and s["mer"] is None and s["rtf"] is None


def test_summary_cer_counts_errors_against_reference():
    rows = [{"reference": "十点开会", "hypothesis": "十点开回", "audio_ms": 1000, "elapsed_ms": 50}]
    s = engine_summary(rows)
    assert 0.2 < s["cer"] <= 0.26  # 1 字错 / 4 字
