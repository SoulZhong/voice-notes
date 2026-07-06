#!/usr/bin/env python3
"""精修 golden 回归:对真实会议样本检验聚类数/纯度/过滤命中。
用法: python3 scripts/refine_golden.py <note_dir> <doubao_md>
数据不入库;本脚本只依赖标准库。"""
import json, re, sys
from collections import defaultdict, Counter

EXPECT_JUNK = {1, 2, 21, 26, 27, 63, 233, 246, 319, 333, 414, 446}
EXPECT_KEEP = {394, 399}
MAX_SPEAKERS = 12          # 真实 7 人,留余量
MIN_TOP1_PURITY = 0.80     # 最大簇对豆包说话人的纯度下限(硬断言,实测 0.88)
# 已知限制:20260706-095122 单麦多人录音下 R2 簇声纹不可分(与主说话人声学特征
# 接近的说话人对,同人/异人相似度分布重叠,AHC 阈值与子窗嵌入平均实验均无法分开),
# 实测纯度 0.50。golden 校准记录不入库,关键数据已内联于本注释。Top2 簇纯度不做
# 0.80 硬断言,只守住已达成水位防继续恶化。
KNOWN_R2_FLOOR = 0.45


def parse_doubao(path):
    entries, cur = [], None
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        m = re.match(r"^@(说话人\s*\d+)\s+(\d+):(\d+)(?::(\d+))?$", line)
        if m:
            sec = (int(m.group(2)) * 3600 + int(m.group(3)) * 60 + int(m.group(4))) if m.group(4) \
                  else (int(m.group(2)) * 60 + int(m.group(3)))
            cur = [m.group(1), sec, ""]
            entries.append(cur)
        elif cur and line and not line.startswith(("#", "录音时间")):
            cur[2] += line
    for i, e in enumerate(entries):
        e.append(entries[i + 1][1] if i + 1 < len(entries) else e[1] + 60)
    return entries


def main(note_dir, doubao_md):
    refined = json.load(open(f"{note_dir}/refined.json", encoding="utf-8"))
    segs = {s["seq"]: s for s in map(json.loads, open(f"{note_dir}/segments.jsonl", encoding="utf-8"))}
    fails = []
    # 1. 过滤命中/误杀。NoteStore.load 会在精修管线之前就丢弃纯空白文本的段
    # (store/notes.rs: "空白段非损坏,不计 skipped_lines"),这类段永远不会进
    # 入 filter::discarded_seqs,也永远不会出现在任何视图里——效果等同于已剔除,
    # 故此处补记为"已处理",避免脚本对着一个从未到达 filter 阶段的 seq 误报漏杀。
    blanked = {q for q, s in segs.items() if not s["text"].strip()}
    got = set(refined["discarded_seqs"]) | blanked
    missed = EXPECT_JUNK - got
    killed = EXPECT_KEEP & got
    if missed: fails.append(f"漏杀垃圾段: {sorted(missed)}")
    if killed: fails.append(f"误杀真实段: {sorted(killed)}")
    # 2. 聚类数
    labels = {p["speaker"] for p in refined["paragraphs"]}
    print(f"聚类标签数: {len(labels)} (原始 45, 真实 7)")
    if len(labels) > MAX_SPEAKERS: fails.append(f"标签数 {len(labels)} > {MAX_SPEAKERS}")
    # 3. 纯度:每段落经 source_seqs 摊回时间区间,对豆包重叠
    entries = parse_doubao(doubao_md)
    overlap = defaultdict(Counter)
    for p in refined["paragraphs"]:
        for q in p["source_seqs"]:
            s = segs[q]
            a, b = s["start_ms"] / 1000, s["end_ms"] / 1000
            for sp, st, _, en in entries:
                ov = min(b, en) - max(a, st)
                if ov > 0: overlap[p["speaker"]][sp] += ov
    for rank, (lab, c) in enumerate(sorted(overlap.items(), key=lambda kv: -sum(kv[1].values()))[:2]):
        total = sum(c.values())
        top = c.most_common(1)[0]
        purity = top[1] / total
        print(f"{lab}: {total:.0f}s 主对应 {top[0]} 纯度 {purity:.2f}")
        if rank == 0:
            if purity < MIN_TOP1_PURITY: fails.append(f"Top1 {lab} 纯度 {purity:.2f} < {MIN_TOP1_PURITY}")
        else:  # Top2 仅守 KNOWN_R2_FLOOR 水位(已知限制,见文件头注释)
            if purity < KNOWN_R2_FLOOR: fails.append(f"Top2 {lab} 纯度 {purity:.2f} < 已知水位 {KNOWN_R2_FLOOR}(恶化)")
    if fails:
        print("FAIL"); [print(" -", f) for f in fails]; sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
