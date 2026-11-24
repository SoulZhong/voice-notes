#!/bin/bash
# #847 一次性修复:把数据目录迁移时漏搬的图谱账本等数据从旧根接回新根。
# 只增不删:新根现有文件先备份,旧根原样保留。必须在应用退出后运行。
#
# 用法: tools/restore-graph-ledger-847.sh <旧数据根> <新数据根> <graph_rebuild 可执行文件>
set -euo pipefail
OLD="$1"; NEW="$2"; REBUILD="$3"
[ -f "$OLD/knowledge-overrides.json" ] || { echo "旧根没有账本: $OLD"; exit 1; }
[ -d "$NEW/notes" ] || { echo "新根没有 notes: $NEW"; exit 1; }
# 主进程不带参数;带 "mcp serve" 的是各终端的 MCP 转发进程,不持有数据。
if pgrep -fl "voice-notes.app/Contents/MacOS/voice-notes" | grep -v "mcp serve" >/dev/null; then
  echo "应用还在运行,先退出再跑"; exit 1
fi
BK="$NEW/backup-before-ledger-restore-$(date +%Y%m%d-%H%M%S)"
mkdir "$BK"
# 1. 备份新根现有的图谱文件(索引可重建,账本是这次要换掉的)
for f in knowledge-overrides.json knowledge-overrides.json.bak graph.sqlite graph.sqlite.previous; do
  [ -e "$NEW/$f" ] && cp -p "$NEW/$f" "$BK/"
done
# 2. 换回旧账本;旧的 .bak 作废(它对应的是被换掉的那份)
cp -p "$OLD/knowledge-overrides.json" "$NEW/knowledge-overrides.json"
rm -f "$NEW/knowledge-overrides.json.bak"
# 3. 其余漏搬的数据:只补新根没有的,绝不覆盖
for d in merge_journal split_ops dictations ai_logs; do
  [ -d "$OLD/$d" ] && mkdir -p "$NEW/$d" && cp -Rpn "$OLD/$d/" "$NEW/$d/"
done
for f in banned_samples.json centroid_receipts.jsonl speaker_truth.jsonl; do
  [ -e "$OLD/$f" ] && [ ! -e "$NEW/$f" ] && cp -p "$OLD/$f" "$NEW/$f"
done
# 4. 按接回的账本重建图谱索引
"$REBUILD" "$NEW"
echo "完成。备份在 $BK"
