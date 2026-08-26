//! 历史场次的场景批量回填(issue #169):把停录时才会写的 scene.json 补给
//! 场景功能(2026-08-23)上线前的存量笔记——没有它,笔记页的场景通知与
//! 「选中可疑段」清理动线对老场次不可见,同源双路的历史污染无从下手。
//!
//! 做法:重放 `SceneSensor`(与录制期同一份判定/防抖逻辑,不复刻),但**不追求
//! 逐事件复刻活体**——那不可达也不必要:重转写/修复过的场次 segments.jsonl 已被
//! 离线重写(长段合并、seq 重排),与活体喂过的子段毫无对应;codex review 第一版
//! 也实锤了「按 seq 回放」会因 mic hold 落盘晚而错序。分类正确性才是目标,离线
//! 有全局视野,取以下口径:
//!
//! - **喂所有 segments.jsonl 行,含被抑制段。** 活体在 `push_mic_sub`/
//!   `push_system_sub` 入口喂传感器(发声口径),语言过滤/无内容/回声抑制都发生
//!   在喂之后;被抑制段同样落盘(sidecar 只记 {seq,reason},实证 144/144 的抑制
//!   seq 都在 segments.jsonl 里)。跳过它们才是偏离。
//! - **重叠按全局 system 时间线算**(合并区间后精确求交),经
//!   `feed_mic_precomputed` 喂入——因果式 sys_windows 在乱序/粗粒度数据上会
//!   系统性漏算(实测把 4 场重转写过的同源双路全判成 headset)。
//! - **段切 ≤10s 片再喂**:重转写产物的长段会把整段时长砸进单个桶,扭曲滑窗
//!   活跃度;切片还原活体子段量级,判定对段粒度不敏感。片按 (end_ms, seq) 排序
//!   驱动传感器时钟。
//! - **echo hit 只对 echo_match/aec_residue/residue_filler,不含 echo_retract**
//!   (活体的 EchoRetract 路径只发撤回事件、不喂 hit——codex P1);hit 在该段
//!   末片之后喂。
//!
//! **残余偏差(刻意,记录在 doc.backfilled 标记下):**
//! - erle 离线读不到 → 恒 None。判定规则有明确降级(倾向报更严重的同源双路);
//!   且 #169 的目标场次实测 erle≈0.18dB,None 与真值同向。
//! - 重叠用全局视野,活体只有因果视野:回放对「system 定稿晚于 mic」的重叠比
//!   活体多算——更接近"同时在说话"的物理事实,但**与活体判定可能分歧**:实测
//!   一场活体判 headset 的直录会议,回放判 dual_path(活体因果序漏算了在途
//!   system 的重叠,且活体还有 erle 佐证)。--verify 对直录对照出现
//!   headset↔dual_path 级分歧属此已知类;回填产物带 backfilled 标记、仅作
//!   清理动线的入口,最终裁决在用户耳朵。4 场重转写对照(重叠 69~98%)不受影响。
//!
//! 保真度不靠论证靠实测:`--verify` 对**有活体 scene.json** 的场次重放并比对
//! final_scene,报告完整分母(总数/可重放/一致/分歧/不可重放),有分歧即非零退出。
//! `--apply` 绝不覆盖:目标路径已存在即跳过,写入用 hard_link 原子占位(已存在
//! 则失败),与「活体正在写」的竞态也安全;解析残缺的场次不写、计入错误并非零退出。
//!
//! 用法:
//!   scene_backfill <data_root>            # dry-run:打印每场判定,不落盘
//!   scene_backfill <data_root> --apply    # 给缺 scene.json 的场次原子写入
//!   scene_backfill <data_root> --verify   # 保真度对照(与 --apply 互斥)
//!   末尾可跟 note_id 列表限定范围。

use std::collections::BTreeMap;
use std::path::Path;

use app_lib::scene::{SceneSensor, SC_DUAL_PATH, SC_SPEAKER_ECHO, SCENE_FILE};

#[derive(serde::Deserialize)]
struct Seg {
    seq: u64,
    source: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(serde::Deserialize)]
struct Sup {
    seq: u64,
    reason: String,
}

/// 活体喂 hit 的三条路径对应的 sidecar reason(session.rs 的 feed_echo_hit 调用点);
/// echo_retract 活体不喂,foreign_language/no_content 亦不喂。
fn is_echo_hit_reason(r: &str) -> bool {
    matches!(r, "echo_match" | "aec_residue" | "residue_filler")
}

/// 读 JSONL。文件缺失 = Ok(空)(抑制 sidecar 本就可选);I/O 错误上抛;
/// 坏行**计数**而非静默吞——残缺输入不许写出权威 scene.json(codex P1)。
fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<(Vec<T>, usize)> {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let (mut out, mut bad) = (Vec::new(), 0usize);
    for l in s.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str(l) {
            Ok(v) => out.push(v),
            Err(_) => bad += 1,
        }
    }
    Ok((out, bad))
}

/// 重放一场。Ok(None) = 无可喂的段(空场);Err = 输入残缺/不可读,调用方计错。
fn replay(note_dir: &Path) -> Result<Option<app_lib::scene::SceneDoc>, String> {
    let (mut segs, bad_seg): (Vec<Seg>, usize) = read_jsonl(&note_dir.join("segments.jsonl"))
        .map_err(|e| format!("segments.jsonl 读取失败: {e}"))?;
    let (sups, bad_sup): (Vec<Sup>, usize) =
        read_jsonl(&note_dir.join("segment-suppressions.jsonl"))
            .map_err(|e| format!("segment-suppressions.jsonl 读取失败: {e}"))?;
    if bad_seg + bad_sup > 0 {
        return Err(format!("坏行 segments={bad_seg} suppressions={bad_sup},拒绝按残缺输入判定"));
    }
    if segs.is_empty() {
        return Ok(None);
    }
    // 来源白名单(codex P1):MixedInput 重转写会把整场写成 source="mixed",此时
    // system 时间线恒空,按 mic 喂会把双路场稳定误判成 onsite 并落成权威 scene.json。
    // 分轨信息已不可恢复 → 明确不可重放,而不是给个错的答案。
    let unknown: usize = segs.iter().filter(|s| s.source != "mic" && s.source != "system").count();
    if unknown > 0 {
        let mut kinds: Vec<&str> = segs
            .iter()
            .filter(|s| s.source != "mic" && s.source != "system")
            .map(|s| s.source.as_str())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        return Err(format!(
            "含 {unknown} 段非 mic/system 来源({}):无分轨信息,双路场景无从判定",
            kinds.join(",")
        ));
    }
    // 时间戳契约(codex P2/P1):逆序即坏输入;绝对值也要设界——合法 JSON 里一个
    // u64::MAX 级 end_ms 会让 SceneSensor::roll 按 30s 步进爬到天荒地老(准无限
    // 循环),超长跨度则切出海量片 OOM。48h 远超任何真实会议,越界按不可重放计。
    const MAX_REASONABLE_MS: u64 = 48 * 3600 * 1000;
    if let Some(bad) = segs.iter().find(|s| s.end_ms < s.start_ms) {
        return Err(format!("seq {} 时间戳逆序({}>{}),坏输入拒判", bad.seq, bad.start_ms, bad.end_ms));
    }
    if let Some(bad) = segs.iter().find(|s| s.end_ms > MAX_REASONABLE_MS) {
        return Err(format!("seq {} 时间戳越界({}ms > 48h),坏输入拒判", bad.seq, bad.end_ms));
    }
    segs.sort_by_key(|s| (s.end_ms, s.seq));
    // 全局 system 时间线:排序+合并重叠区间,mic 重叠对它精确求交(见模块头)。
    let mut sys_iv: Vec<(u64, u64)> = segs
        .iter()
        .filter(|s| s.source == "system")
        .map(|s| (s.start_ms, s.end_ms))
        .collect();
    sys_iv.sort_unstable();
    let mut sys_merged: Vec<(u64, u64)> = Vec::with_capacity(sys_iv.len());
    for (a, b) in sys_iv {
        match sys_merged.last_mut() {
            Some((_, e)) if a <= *e => *e = (*e).max(b),
            _ => sys_merged.push((a, b)),
        }
    }
    let ov_of = |a: u64, b: u64| -> u64 {
        // sys_merged 有序不相交:二分到第一个可能相交的区间起扫。
        let i = sys_merged.partition_point(|(_, e)| *e <= a);
        sys_merged[i..]
            .iter()
            .take_while(|(s, _)| *s < b)
            .map(|(s, e)| b.min(*e).saturating_sub(a.max(*s)))
            .sum()
    };
    // seq → reason:按 seq 去重(writer 侧本有 existing 去重,这里防御重复行);
    // 同 seq 不同 reason 属异常,告警并取先见者。
    let mut reason_of: BTreeMap<u64, String> = BTreeMap::new();
    for s in sups {
        if let Some(prev) = reason_of.get(&s.seq) {
            if *prev != s.reason {
                eprintln!(
                    "  警告 {}: seq {} 抑制原因冲突 {prev:?} vs {:?},取先见者",
                    note_dir.display(),
                    s.seq,
                    s.reason
                );
            }
            continue;
        }
        reason_of.insert(s.seq, s.reason);
    }
    // 切 ≤10s 片(见模块头),片按 (end_ms, seq) 排序驱动传感器时钟。
    const CHUNK_MS: u64 = 10_000;
    struct Piece {
        end_ms: u64,
        start_ms: u64,
        seq: u64,
        is_system: bool,
        echo_hit: bool, // 段末片承载该段的回声命中
    }
    let mut pieces: Vec<Piece> = Vec::new();
    let mut max_end = 0u64;
    for seg in &segs {
        let is_system = seg.source == "system";
        let hit = reason_of.get(&seg.seq).is_some_and(|r| is_echo_hit_reason(r));
        let mut a = seg.start_ms;
        let end = seg.end_ms; // 逆序已在上方拒绝
        loop {
            let b = a.saturating_add(CHUNK_MS).min(end);
            pieces.push(Piece { end_ms: b, start_ms: a, seq: seg.seq, is_system, echo_hit: hit && b == end });
            if b >= end {
                break;
            }
            a = b;
        }
        max_end = max_end.max(end);
    }
    pieces.sort_by_key(|p| (p.end_ms, p.seq));
    let mut sensor = SceneSensor::new();
    for p in &pieces {
        if p.is_system {
            sensor.feed_system(p.start_ms, p.end_ms, None);
        } else {
            // mic 与其余来源(mixed 等历史口径)按 mic 喂:发声口径,重叠取全局精确值。
            sensor.feed_mic_precomputed(p.start_ms, p.end_ms, ov_of(p.start_ms, p.end_ms), None);
        }
        if p.echo_hit {
            sensor.feed_echo_hit();
        }
    }
    let mut doc = sensor.finish(max_end);
    doc.backfilled = true;
    Ok(Some(doc))
}

/// 原子 no-clobber 写:内容先落 tmp,hard_link 到目标(已存在即失败,与并发写者
/// 无竞态窗口),再删 tmp。比「exists 检查 + rename」强:rename 在 Unix 上无条件
/// 覆盖,检查与改名之间的窗口能吃掉活体刚写的文件(codex P1)。
fn write_no_clobber(note_dir: &Path, doc: &app_lib::scene::SceneDoc) -> Result<(), String> {
    let target = note_dir.join(SCENE_FILE);
    // 唯一临时名 + create_new(codex P1):固定名会让两个并发回填共写同一 tmp,
    // 其一 link 成功后另一个还握着同 inode 继续写,等于隔空改写已发布文件。
    let tmp = note_dir.join(format!("{SCENE_FILE}.backfill.{}.tmp", std::process::id()));
    let json = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    // 建成 tmp 后任何退出路径都要清理(codex P2):写失败经 ? 直接返回会留半截
    // PID 名临时文件,PID 复用后 create_new 从此恒失败。
    let write_result = (|| -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("建 tmp 失败: {e}"))?;
        f.write_all(json.as_bytes()).map_err(|e| format!("写 tmp 失败: {e}"))
    })();
    if let Err(e) = write_result {
        if std::fs::remove_file(&tmp).is_err() && tmp.exists() {
            eprintln!("  警告 {}: 写失败后 tmp 清理也失败(残留 {})", note_dir.display(), tmp.display());
        }
        return Err(e);
    }
    let linked = std::fs::hard_link(&tmp, &target);
    if let Err(e) = std::fs::remove_file(&tmp) {
        eprintln!("  警告 {}: tmp 清理失败(残留 {}): {e}", note_dir.display(), tmp.display());
    }
    linked.map_err(|e| format!("目标已存在或链接失败(不覆盖): {e}"))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        eprintln!("用法: scene_backfill <data_root> [--apply|--verify] [note_id ...]");
        std::process::exit(2);
    };
    let (mut apply, mut verify, mut only) = (false, false, Vec::<String>::new());
    for a in args {
        match a.as_str() {
            "--apply" => apply = true,
            "--verify" => verify = true,
            _ if a.starts_with("--") => {
                eprintln!("未知选项: {a}");
                std::process::exit(2);
            }
            _ => only.push(a),
        }
    }
    if apply && verify {
        eprintln!("--apply 与 --verify 互斥");
        std::process::exit(2);
    }

    let notes_dir = Path::new(&root).join("notes");
    let mut ids: Vec<String> = match std::fs::read_dir(&notes_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) => {
            eprintln!("读不到 {notes_dir:?}: {e}");
            std::process::exit(2);
        }
    };
    ids.sort();
    if !only.is_empty() {
        let missing: Vec<&String> = only.iter().filter(|o| !ids.contains(o)).collect();
        if !missing.is_empty() {
            eprintln!("指定的 note_id 不存在: {missing:?}");
            std::process::exit(2);
        }
        ids.retain(|id| only.contains(id));
    }

    let mut errors: Vec<String> = Vec::new();
    if verify {
        // 保真度对照:完整分母——有活体 doc 的总数 = 一致 + 分歧 + 不可重放。
        let (mut total, mut agree, mut differ, mut unreplayable) =
            (0usize, 0usize, Vec::new(), Vec::new());
        for id in &ids {
            let dir = notes_dir.join(id);
            let path_exists = dir.join(SCENE_FILE).exists();
            if !path_exists {
                continue;
            }
            // 对照读取也要稳定快照(codex P2):续录不会删旧 scene.json,录制中的场
            // 拿旧活体 doc 比增长中的 segments 会造伪分歧。非 complete/被占用 →
            // 计入不可重放而不是静默跳过。
            let meta_ok = std::fs::read_to_string(dir.join("meta.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("state").and_then(|x| x.as_str().map(String::from)))
                .as_deref()
                == Some("complete");
            if !meta_ok {
                total += 1;
                unreplayable.push(format!("  {id}: 非 complete 态,快照不稳定"));
                continue;
            }
            let _flock = match app_lib::store::notelock::NoteLock::acquire(&dir) {
                Ok(Some(l)) => l,
                _ => {
                    total += 1;
                    unreplayable.push(format!("  {id}: 笔记正被占用,快照不稳定"));
                    continue;
                }
            };
            let live = app_lib::scene::load(&dir);
            let Some(live) = live else {
                // load 把「缺失」与「损坏」都折叠成 None(codex P2):按路径区分,
                // 损坏的活体文件计入分母——静默缩水会让 0=0+0+0 假装全绿。
                total += 1;
                unreplayable.push(format!("  {id}: scene.json 存在但解析失败(损坏)"));
                continue;
            };
            if live.backfilled {
                continue; // 之前回填的不算活体对照
            }
            total += 1;
            match replay(&dir) {
                Ok(Some(replayed)) if replayed.final_scene == live.final_scene => agree += 1,
                Ok(Some(replayed)) => differ
                    .push(format!("  {id}: 活体={} 重放={}", live.final_scene, replayed.final_scene)),
                Ok(None) => unreplayable.push(format!("  {id}: 无可喂段")),
                Err(e) => unreplayable.push(format!("  {id}: {e}")),
            }
        }
        println!("保真度: 活体对照 {total} 场 = 一致 {agree} + 分歧 {} + 不可重放 {}",
            differ.len(), unreplayable.len());
        for d in differ.iter().chain(unreplayable.iter()) {
            println!("{d}");
        }
        if total == 0 {
            eprintln!("对照集为空:没有任何活体 scene.json 可比,验证无从谈起");
            std::process::exit(1);
        }
        if !differ.is_empty() || !unreplayable.is_empty() {
            std::process::exit(1); // 验证不完整/有分歧:非零,别让 CI/调用方误读为全绿
        }
        return;
    }

    let (mut done, mut skipped, mut suspicious) = (0usize, 0usize, Vec::new());
    for id in &ids {
        let dir = notes_dir.join(id);
        if dir.join(SCENE_FILE).exists() {
            skipped += 1; // 活体记录优先;存在性按路径判,不拿"能否反序列化"当依据
            continue;
        }
        // 只收尾完成态(codex P1):recording/中断态的 segments 还会被续录/收尾改写,
        // 读到哪代数据全凭运气,回填结果不可信。
        let meta_state = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("state").and_then(|x| x.as_str().map(String::from)));
        if meta_state.as_deref() != Some("complete") {
            errors.push(format!("  {id}: meta.state={:?},非 complete 不回填", meta_state));
            continue;
        }
        // NoteLock 覆盖「读输入→链接目标」全程(codex P1):否则录制/重转写原子替换
        // 期间可能读到两个文件的不同代组合,坏行检查发现不了半行截断以外的错配。
        let _flock = match app_lib::store::notelock::NoteLock::acquire(&dir) {
            Ok(Some(l)) => l,
            Ok(None) => {
                errors.push(format!("  {id}: 笔记正被占用(录制/转码/重转写),跳过"));
                continue;
            }
            Err(e) => {
                errors.push(format!("  {id}: 笔记锁不可用: {e}"));
                continue;
            }
        };
        let doc = match replay(&dir) {
            Ok(Some(doc)) => doc,
            Ok(None) => continue,
            Err(e) => {
                errors.push(format!("  {id}: {e}"));
                continue;
            }
        };
        let flag = if doc.final_scene == SC_DUAL_PATH || doc.final_scene == SC_SPEAKER_ECHO {
            suspicious.push(id.clone());
            "  ← 可疑"
        } else {
            ""
        };
        println!("{id}: {} (窗 {}){flag}", doc.final_scene, doc.windows.len());
        if apply {
            match write_no_clobber(&dir, &doc) {
                Ok(()) => done += 1,
                Err(e) => errors.push(format!("  {id}: {e}")),
            }
        } else {
            done += 1;
        }
    }
    println!(
        "\n{}处理 {done} 场,跳过(已有 scene.json) {skipped} 场,错误 {} 场",
        if apply { "已写入 " } else { "dry-run " },
        errors.len()
    );
    for e in &errors {
        println!("{e}");
    }
    if !suspicious.is_empty() {
        println!("判为 同源双路/外放回声 的历史场次({}):", suspicious.len());
        for id in &suspicious {
            println!("  {id}");
        }
        println!("(打开对应笔记即可见场景通知与「选中可疑段」清理动线)");
    }
    if !errors.is_empty() {
        std::process::exit(1);
    }
}
