//! 合并日志:声纹合并的"可撤销"层。merge 本身不可逆(质心加权平均、样本超额永久
//! 删除、redirect 压扁),因此每条合并前把双方 Person 快照 + 样本文件副本落到
//! merge_journal/<id>/,撤销=按快照还原(还原逻辑在 voiceprints.rs undo_merge,
//! 那里有库的 load/save/guard)。另存自动合并拒绝名单:撤销过的 pair 落盘,自动
//! 归并不再碰——重启也不犯同样的错。
//! 设计:docs/superpowers/specs/2026-07-31-speaker-tidy-redesign-design.md
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::voiceprints::Person;

/// 日志条目上限:超出按 time 淘汰最旧(连同样本副本)。50 条远超一轮整理的量,
/// 上限只是防无界膨胀。
pub const JOURNAL_CAP: usize = 50;

/// 恢复用的临时文件计数器,保证同一目标的临时名不互撞。
static RESTORE_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 把 `src` 拷到 `dst`,**目标已存在就跳过、绝不覆盖**。返回 true=本次拷了。
///
/// 做法:先写唯一临时文件,再 `hard_link` 到目标。`hard_link` 在目标已存在时原子
/// 失败,所以不存在"先 exists 再 copy"的时间窗。失败路径一律清掉临时文件,目标位置
/// 不会留下半个文件。
///
/// 为什么不能直接 `fs::copy`:拆回/撤销在样本恢复失败后会保留 journal 让用户重试,
/// 而用户可能在重试前又录了新样本。`fs::copy` 会拿旧快照把这份新录音盖掉
/// (codex review 实现轮二 P1)。
fn copy_no_overwrite(src: &Path, dst: &Path) -> anyhow::Result<bool> {
    let seq = RESTORE_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dst.with_extension(format!("restore-{}-{seq}.tmp", std::process::id()));
    if let Err(e) = std::fs::copy(src, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("拷贝样本副本失败({} → 临时文件): {e}", src.display()));
    }
    let linked = match std::fs::hard_link(&tmp, dst) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow::anyhow!("落位样本失败({}): {e}", dst.display())),
    };
    let _ = std::fs::remove_file(&tmp);
    linked
}

/// `copy_no_overwrite` 的按目录版:目标名沿用源文件名。
fn restore_one_sample(src: &Path, vp_samples_dir: &Path) -> anyhow::Result<bool> {
    let name = src
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("样本副本路径无文件名: {}", src.display()))?;
    copy_no_overwrite(src, &vp_samples_dir.join(name))
}

/// 人工处置键名单上限:超出按追加序淘汰最旧。500 远超一轮整理量,只为防无界
/// 膨胀——丢了顶多让用户重启后再处置一次。
pub const DISMISSED_CAP: usize = 500;

/// 一条合并日志。id 格式 `m-<loser>`:loser 合并后即从库中消失,同一 loser 不会
/// 有两条并存(撤销会删条目,重新合并再落新条),天然唯一。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeJournalEntry {
    pub id: String,
    /// RFC3339 落盘时间(排序/淘汰用,由命令层传入)。
    pub time: String,
    /// "auto" | "manual"。auto 且未确认的条目进审阅流回执卡。
    pub origin: String,
    pub loser: String,
    pub winner: String,
    /// 合并前双方显示名(回执展示用;合并后 loser 已不在库里,无从回查)。
    pub loser_name: String,
    pub winner_name: String,
    #[serde(default)]
    pub similarity: Option<f32>,
    /// 合并前双方完整记录(撤销恢复用)。
    pub loser_person: Person,
    pub winner_person: Person,
    /// 合并前指向 loser 的 redirect 键(merge 压扁会改指 winner,撤销要还原)。
    #[serde(default)]
    pub redirects_to_loser: Vec<String>,
    /// 已确认(回执卡点「好」后删除条目,此字段区分 manual 生来已确认)。
    #[serde(default)]
    pub acknowledged: bool,
    /// 失效原因;None=可撤销。失效条目留着给回执卡说明原因,确认后才删。
    #[serde(default)]
    pub invalid_reason: Option<String>,
    /// 使其失效的后续合并 journal id(仅失效源是另一次合并时有值):撤销那次合并
    /// 时本条复活——链式撤销 LIFO 可行的关键。
    #[serde(default)]
    pub invalidated_by: Option<String>,
    /// 快照里那两份 `Person` 的质心属于哪个模型空间(落盘时库的 `embedding_model`)。
    ///
    /// **没有它,快照回放就是一个绕过所有门禁的后门**:换模型重建之后,
    /// `rebuild_for_model` 会把全部条目标记失效,而「拆回人物」**恰恰只接受失效条目**
    /// ——于是任何一条老日志都能把旧空间的质心原样插回新库,库标签还是新的
    /// (2026-08-19 codex review 设计轮 P1)。
    ///
    /// 旧条目没有这一栏(空串)= 来源不明,回放时按"只还身份、质心置空"处理。
    #[serde(default)]
    pub embedding_model: String,
}

/// root 为 app_data_dir(与 VoiceprintStore 同根),日志落 root/merge_journal/。
/// 并发:凡触碰条目目录(entries/样本副本/denylist)的调用方都必须在 vp_guard
/// 内操作,模块自身不再加锁。命令层异步化(spawn_blocking)后不再有"单线程命令
/// 上下文"这条豁免——直连写条目的旧路径(如 acknowledge)已收编进 VoiceprintStore
/// 持锁包装,新调用方一律走那里。例外:dismissed 处置名单是独立单写文件,与条目
/// 目录不相交,dismiss_item 免锁直连仍安全。
pub struct MergeJournal {
    root: PathBuf,
    #[cfg(test)]
    fail_next_save_for: std::sync::Mutex<Option<String>>,
}

impl MergeJournal {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            #[cfg(test)]
            fail_next_save_for: std::sync::Mutex::new(None),
        }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("merge_journal")
    }

    /// 防御 IPC 构造路径:id 只允许 ASCII 字母数字与 '-'。
    fn entry_dir(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        Some(self.dir().join(id))
    }

    fn entry_path(&self, id: &str) -> Option<PathBuf> {
        self.entry_dir(id).map(|d| d.join("entry.json"))
    }

    fn samples_dir(&self, id: &str, side: &str) -> Option<PathBuf> {
        self.entry_dir(id).map(|d| d.join("samples").join(side))
    }

    pub fn entry(&self, id: &str) -> anyhow::Result<MergeJournalEntry> {
        let p = self.entry_path(id).ok_or_else(|| anyhow::anyhow!("非法日志 id: {id}"))?;
        let s = std::fs::read_to_string(&p).map_err(|_| anyhow::anyhow!("日志条目不存在: {id}"))?;
        Ok(serde_json::from_str(&s)?)
    }

    /// 全部条目,time 降序(新的在前)。损坏条目跳过(eprintln)——日志是增值层,
    /// 单条坏不该挡整理。
    pub fn entries(&self) -> Vec<MergeJournalEntry> {
        let Ok(rd) = std::fs::read_dir(self.dir()) else { return vec![] };
        let mut out: Vec<MergeJournalEntry> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                // fail-closed 失效路径会先把目录原子改名为隐藏 quarantine。只读取
                // 路径名与 entry.id 严格一致的合法目录,避免隔离条目重新进入 UI。
                let dir_id = e.file_name().to_str()?.to_string();
                if self.entry_dir(&dir_id).as_deref() != Some(e.path().as_path()) {
                    return None;
                }
                let s = std::fs::read_to_string(e.path().join("entry.json")).ok()?;
                match serde_json::from_str::<MergeJournalEntry>(&s) {
                    Ok(v) if v.id == dir_id => Some(v),
                    Ok(_) => {
                        eprintln!("合并日志目录名与条目 id 不一致,跳过({:?})", e.path());
                        None
                    }
                    Err(err) => {
                        eprintln!("合并日志条目损坏,跳过({:?}): {err}", e.path());
                        None
                    }
                }
            })
            .collect();
        out.sort_by(|a, b| b.time.cmp(&a.time));
        out
    }

    /// 原子写条目 JSON(tmp+rename,与库文件同哲学)。
    fn save_entry(&self, e: &MergeJournalEntry) -> anyhow::Result<()> {
        #[cfg(test)]
        {
            let mut fail = self.fail_next_save_for.lock().expect("测试 failpoint 锁");
            if fail.as_deref() == Some(e.id.as_str()) {
                fail.take();
                anyhow::bail!("测试注入:save_entry 失败");
            }
        }
        let path = self.entry_path(&e.id).ok_or_else(|| anyhow::anyhow!("非法日志 id: {}", e.id))?;
        std::fs::create_dir_all(path.parent().expect("entry_path 恒有父目录"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(e)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 失效标记写不下去时,旧 entry.json 仍表示“可撤销”。先把整个目录原子移出
    /// 合法命名空间,再 best-effort 删除;这样即使删除失败,entry()/entries() 也看
    /// 不到过期快照。rename 不可用时退回直接删除。
    fn discard_stale_entry(&self, id: &str) -> anyhow::Result<()> {
        let dir = self.entry_dir(id).ok_or_else(|| anyhow::anyhow!("非法日志 id: {id}"))?;
        let quarantine = self.dir().join(format!(".invalid-{id}"));
        if quarantine.exists() {
            let _ = std::fs::remove_dir_all(&quarantine);
        }
        match std::fs::rename(&dir, &quarantine) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(quarantine);
                Ok(())
            }
            Err(rename_err) => std::fs::remove_dir_all(&dir)
                .map_err(|remove_err| anyhow::anyhow!("隔离失败:{rename_err};删除失败:{remove_err}")),
        }
    }

    /// 落一条新日志:entry.json + 双方样本文件副本。任一步失败→清掉半成品目录并
    /// Err(调用方据此放弃该条合并——绝不做没有退路的合并)。成功后按上限淘汰最旧。
    pub fn append(
        &self,
        e: &MergeJournalEntry,
        loser_samples: &[PathBuf],
        winner_samples: &[PathBuf],
    ) -> anyhow::Result<()> {
        let res = (|| -> anyhow::Result<()> {
            self.save_entry(e)?;
            for (side, files) in [("loser", loser_samples), ("winner", winner_samples)] {
                let dir = self
                    .samples_dir(&e.id, side)
                    .ok_or_else(|| anyhow::anyhow!("非法日志 id: {}", e.id))?;
                std::fs::create_dir_all(&dir)?;
                for f in files {
                    let name = f.file_name().ok_or_else(|| anyhow::anyhow!("样本路径无文件名: {f:?}"))?;
                    std::fs::copy(f, dir.join(name))?;
                }
            }
            Ok(())
        })();
        if let Err(err) = res {
            if let Some(d) = self.entry_dir(&e.id) {
                let _ = std::fs::remove_dir_all(&d);
            }
            return Err(err);
        }
        self.prune();
        Ok(())
    }

    /// 触及 touched 中任一人物的**有效**条目全部失效。by=使其失效的合并 id(该
    /// 合并被撤销时本条复活);by=None 为永久性失效(改名/删除/新录制等),不可
    /// 复活。两种失效都**保留**样本副本:失效条目一旦不能撤销,回执卡上「合并
    /// 时的原声」就是唯一还能核对的东西,最不该在这时候删掉——空间由 JOURNAL_CAP
    /// 上限与确认/淘汰时随目录一并清理兜底。失效标记写失败时 fail-closed 隔离
    /// 整条日志:宁可少一个可撤销项,也不能让过期快照继续可撤销并覆盖后续人物
    /// 数据。
    pub fn invalidate(&self, touched: &[&str], reason: &str, by: Option<&str>) {
        for mut e in self.entries() {
            if e.invalid_reason.is_some() {
                continue;
            }
            if !touched.contains(&e.loser.as_str()) && !touched.contains(&e.winner.as_str()) {
                continue;
            }
            if Some(e.id.as_str()) == by {
                continue; // 合并自身的条目不被自己失效
            }
            e.invalid_reason = Some(reason.to_string());
            e.invalidated_by = by.map(str::to_string);
            if let Err(err) = self.save_entry(&e) {
                if let Err(discard_err) = self.discard_stale_entry(&e.id) {
                    eprintln!(
                        "合并日志失效标记写入失败且隔离失败({}): {err};{discard_err}",
                        e.id
                    );
                } else {
                    eprintln!("合并日志失效标记写入失败,已隔离过期撤销项({}): {err}", e.id);
                }
                continue;
            }
        }
    }

    /// 全部有效条目失效(声纹库整体重建等场景)。永久失效,样本副本保留(同
    /// invalidate 文档)。
    pub fn invalidate_all(&self, reason: &str) {
        let ids: Vec<String> = self
            .entries()
            .iter()
            .filter(|e| e.invalid_reason.is_none())
            .flat_map(|e| [e.loser.clone(), e.winner.clone()])
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.invalidate(&refs, reason, None);
    }

    /// 由 by_id 那次合并所失效的条目复活(撤销 by_id = 它施加的失效不再成立)。
    pub fn revive_invalidated_by(&self, by_id: &str) {
        for mut e in self.entries() {
            if e.invalidated_by.as_deref() != Some(by_id) {
                continue;
            }
            e.invalid_reason = None;
            e.invalidated_by = None;
            if let Err(err) = self.save_entry(&e) {
                eprintln!("合并日志复活写入失败({}): {err}", e.id);
            }
        }
    }

    /// 确认(回执卡「好」)= 删除条目(连同样本副本)。
    pub fn acknowledge(&self, id: &str) -> anyhow::Result<()> {
        self.remove(id)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _ = self.entry(id)?; // 校验存在与 id 合法性
        let dir = self.entry_dir(id).expect("entry() 已校验合法");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// 超上限淘汰最旧(entries 已按 time 降序)。
    fn prune(&self) {
        let entries = self.entries();
        for e in entries.iter().skip(JOURNAL_CAP) {
            if let Some(d) = self.entry_dir(&e.id) {
                let _ = std::fs::remove_dir_all(&d);
            }
        }
    }

    /// 某侧(loser/winner)合并前的样本快照副本路径,按槽位序。条目不存在/该侧
    /// 无样本 → 空(回执卡隐藏该试听行)。失效条目(无论 by=Some/None)的副本
    /// 保留,直至条目被确认/撤销/淘汰随目录一并清理。
    pub fn sample_copies(&self, id: &str, side: &str) -> Vec<PathBuf> {
        // 面向 UI 的宽松版:读不出来就当没有。恢复路径**不得**用它,见
        // sample_copies_strict(codex review 实现轮二 P1)。
        self.sample_copies_strict(id, side).unwrap_or_default()
    }

    /// 同 `sample_copies`,但**严格传播 I/O 错误**:目录不存在=该侧本就无样本(Ok 空),
    /// 其余读取失败(权限、损坏、单个目录项出错)一律上抛。恢复路径必须用这个——
    /// 宽松版把读失败当"无样本",上层会得到 `Ok(0)` 并删掉 journal,唯一的样本副本
    /// 就永久没了(codex review 实现轮二 P1)。
    fn sample_copies_strict(&self, id: &str, side: &str) -> anyhow::Result<Vec<PathBuf>> {
        let dir = self
            .samples_dir(id, side)
            .ok_or_else(|| anyhow::anyhow!("非法日志 id: {id}"))?;
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(anyhow::anyhow!("读取样本副本目录失败({}): {e}", dir.display())),
        };
        let mut out: Vec<PathBuf> = Vec::new();
        for f in rd {
            let f = f.map_err(|e| anyhow::anyhow!("读取样本副本目录项失败({}): {e}", dir.display()))?;
            out.push(f.path());
        }
        // 槽位序而非字典序:<id>.wav=槽1,<id>-N.wav=槽N。字典序会把 '-' 排在 '.'
        // 前,槽1(最老)反而落到最后,前端"最后一份=最新"的取法就拿错。兜底截声
        // 文件 "<loser>-cut.wav" 的后缀非数字,排在所有数字槽之后(u32::MAX - 1,
        // 留 u32::MAX 给理论上更极端的未知后缀,避免边界互相踩)。
        let slot = |p: &PathBuf| -> u32 {
            match p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.rsplit_once('-')) {
                None => 1,
                Some((_, n)) => n.parse().unwrap_or(u32::MAX - 1),
            }
        };
        out.sort_by_key(slot);
        Ok(out)
    }

    /// 被并入方(loser)合并前的样本快照副本路径,按槽位序。薄委托,保留旧名不破
    /// 既有调用。
    pub fn loser_sample_copies(&self, id: &str) -> Vec<PathBuf> {
        self.sample_copies(id, "loser")
    }

    /// 无样本 loser 的兜底截声写入快照副本(合并时现场从笔记音频截的 loser 原声):
    /// 回执卡左栏据此有得听,不再"无可试听的快照"。best-effort:失败只 eprintln,
    /// 不影响合并(与样本层一贯哲学)。
    pub fn write_loser_cut_sample(&self, id: &str, loser: &str, samples: &[f32]) {
        let res = (|| -> anyhow::Result<()> {
            let Some(dir) = self.samples_dir(id, "loser") else {
                anyhow::bail!("非法日志 id: {id}");
            };
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{loser}-cut.wav"));
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: crate::store::audio::AUDIO_SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let tmp = path.with_extension("wav.tmp");
            let mut w = hound::WavWriter::create(&tmp, spec)?;
            for s in samples {
                w.write_sample(crate::store::audio::f32_to_s16(*s))?;
            }
            w.finalize()?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("合并日志兜底截声写入失败({id},不影响合并): {e}");
        }
    }

    /// 把条目的样本副本拷回声纹样本目录(撤销用),返回**本次实际拷回**的文件数
    /// (目标已存在的跳过、不计数)。任何读写失败都上抛——调用方据此保留 journal。
    pub fn restore_samples(&self, id: &str, vp_samples_dir: &Path) -> anyhow::Result<usize> {
        std::fs::create_dir_all(vp_samples_dir)?;
        let mut n = 0usize;
        for side in ["loser", "winner"] {
            for p in self.sample_copies_strict(id, side)? {
                if restore_one_sample(&p, vp_samples_dir)? {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// 只把 loser 侧快照副本拷回声纹样本目录(拆回用;undo 用 restore_samples 双侧)。
    /// 无常规槽位而有 `<loser>-cut.wav` 兜底截声时,改名拷成 `<loser>.wav`(槽 1)——
    /// sample_slot_path 不识别 -cut 后缀,原名拷回等于拆回的人"无样本"。
    pub fn restore_loser_samples(&self, id: &str, vp_samples_dir: &Path) -> anyhow::Result<usize> {
        std::fs::create_dir_all(vp_samples_dir)?;
        let copies = self.sample_copies_strict(id, "loser")?;
        let is_cut = |p: &PathBuf| {
            p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.ends_with("-cut"))
        };
        let regular: Vec<&PathBuf> = copies.iter().filter(|p| !is_cut(p)).collect();
        let mut n = 0usize;
        for p in &regular {
            if restore_one_sample(p, vp_samples_dir)? {
                n += 1;
            }
        }
        // 兜底截声只在**根本没有常规槽位**时才用。判据是 regular 为空,不是 n==0——
        // 重试时常规槽位可能已全部就位(拷回数 0),那不该再塞一份 -cut
        // (codex review 实现轮二 P1)。
        if regular.is_empty() {
            if let Some(cut) = copies.iter().find(|p| is_cut(p)) {
                let stem = cut.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                let loser = stem.trim_end_matches("-cut");
                if copy_no_overwrite(cut, &vp_samples_dir.join(format!("{loser}.wav")))? {
                    n = 1;
                }
            }
        }
        Ok(n)
    }

    // ── 自动合并拒绝名单(撤销过的 pair,落盘;仅屏蔽自动合并,不屏蔽人工建议) ──

    fn denylist_path(&self) -> PathBuf {
        self.dir().join("auto_denylist.json")
    }

    /// 缺失/损坏 → 空(拒绝名单是保护层,坏了顶多多合一次,撤销还在)。
    pub fn auto_denylist(&self) -> Vec<String> {
        std::fs::read_to_string(self.denylist_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 追加 pair(格式 "loser>winner",与前端 sugKey 一致),去重。best-effort。
    pub fn deny_auto(&self, pair: &str) {
        let mut list = self.auto_denylist();
        if list.iter().any(|p| p == pair) {
            return;
        }
        list.push(pair.to_string());
        let res = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(self.dir())?;
            let tmp = self.denylist_path().with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&list)?)?;
            std::fs::rename(&tmp, self.denylist_path())?;
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("自动合并拒绝名单写入失败: {e}");
        }
    }

    // ── 整理条目人工处置(忽略/保留),落盘 ──

    fn dismissed_path(&self) -> PathBuf {
        self.dir().join("dismissed.json")
    }

    /// 人工处置过的整理条目键(忽略的建议对/保留的无样本条目/忽略的同名组)。
    /// 缺失/损坏 → 空。键格式与前端 tidyItemKey 一致,后端只存不解释。
    pub fn dismissed_items(&self) -> Vec<String> {
        std::fs::read_to_string(self.dismissed_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 追加处置键,去重,超过 DISMISSED_CAP 淘汰最旧。best-effort:丢了顶多重启
    /// 后再展示一次,不影响正确性。
    pub fn dismiss_item(&self, key: &str) {
        let mut list = self.dismissed_items();
        if list.iter().any(|k| k == key) {
            return;
        }
        list.push(key.to_string());
        if list.len() > DISMISSED_CAP {
            let drop = list.len() - DISMISSED_CAP;
            list.drain(0..drop);
        }
        let res = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(self.dir())?;
            let tmp = self.dismissed_path().with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&list)?)?;
            std::fs::rename(&tmp, self.dismissed_path())?;
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("整理条目处置名单写入失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::voiceprints::Person;

    fn entry(id: &str, time: &str, loser: &str, winner: &str) -> MergeJournalEntry {
        MergeJournalEntry {
            id: id.into(),
            time: time.into(),
            origin: "auto".into(),
            loser: loser.into(),
            winner: winner.into(),
            loser_name: String::new(),
            winner_name: "张三".into(),
            similarity: Some(0.8),
            loser_person: Person::default(),
            winner_person: Person::default(),
            redirects_to_loser: vec![],
            acknowledged: false,
            invalid_reason: None,
            invalidated_by: None,
            embedding_model: "campplus".into(),
        }
    }

    fn fake_sample(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"RIFFfake-wav").unwrap();
        p
    }

    /// 建一个空日志(临时根目录 + 实例)。TempDir 需随返回值存活,否则目录被清。
    fn test_journal() -> (tempfile::TempDir, MergeJournal) {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        (tmp, j)
    }

    /// 直接在某条目某侧的样本目录里摆一份快照副本文件,跳过 append() 的完整校验
    /// 流程——只测只读拷贝类方法(如 restore_loser_samples)时够用。
    fn put_side_file(j: &MergeJournal, id: &str, side: &str, name: &str) {
        let dir = j.samples_dir(id, side).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), b"RIFFfake-wav").unwrap();
    }

    #[test]
    fn invalidate_failure_removes_stale_undo_entry_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[], &[]).unwrap();
        *j.fail_next_save_for.lock().unwrap() = Some("m-P1".into());

        j.invalidate(&["P1"], "此人随后被改名", None);

        assert!(j.entry("m-P1").is_err(), "写不下失效标记时旧快照必须不可撤销");
        assert!(j.entries().is_empty(), "隔离目录不能重新进入回执/撤销列表");
    }

    #[test]
    fn append_and_entries_roundtrip_with_sample_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ls = fake_sample(&vpdir, "P1.wav");
        let ws = fake_sample(&vpdir, "P2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[ls], &[ws]).unwrap();

        let got = j.entry("m-P1").unwrap();
        assert_eq!(got.winner, "P2");
        assert!(tmp.path().join("merge_journal/m-P1/samples/loser/P1.wav").exists());
        assert!(tmp.path().join("merge_journal/m-P1/samples/winner/P2.wav").exists());
        assert_eq!(j.entries().len(), 1);
    }

    #[test]
    fn entries_sorted_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "2026-07-31T10:00:00+08:00", "P1", "P9"), &[], &[]).unwrap();
        j.append(&entry("m-P2", "2026-07-31T11:00:00+08:00", "P2", "P9"), &[], &[]).unwrap();
        let ids: Vec<String> = j.entries().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["m-P2", "m-P1"]);
    }

    #[test]
    fn append_fails_cleanly_when_sample_copy_impossible() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let missing = tmp.path().join("voiceprints/none.wav"); // 不存在
        assert!(j.append(&entry("m-P1", "t", "P1", "P2"), &[missing], &[]).is_err());
        assert!(!tmp.path().join("merge_journal/m-P1").exists(), "半成品目录必须清掉");
        assert!(j.entries().is_empty());
    }

    #[test]
    fn invalidate_marks_touched_skips_by_id_and_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[], &[]).unwrap();
        j.append(&entry("m-P2", "t2", "P2", "P8"), &[], &[]).unwrap();
        j.append(&entry("m-P3", "t3", "P3", "P9"), &[], &[]).unwrap();

        j.invalidate(&["P9"], "相关人物随后又被合并", Some("m-P3"));
        assert!(j.entry("m-P1").unwrap().invalid_reason.is_some());
        assert_eq!(j.entry("m-P1").unwrap().invalidated_by.as_deref(), Some("m-P3"));
        assert!(j.entry("m-P2").unwrap().invalid_reason.is_none(), "未触及的不失效");
        assert!(j.entry("m-P3").unwrap().invalid_reason.is_none(), "by 自身除外");
    }

    #[test]
    fn invalidate_keeps_sample_copies_regardless_of_cause() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let s1 = fake_sample(&vpdir, "P1.wav");
        let s2 = fake_sample(&vpdir, "P2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[s1], &[]).unwrap();
        j.append(&entry("m-P2", "t2", "P2", "P9"), &[s2], &[]).unwrap();

        // 由另一次合并失效(可复活):样本副本保留
        j.invalidate(&["P1"], "相关人物随后又被合并", Some("m-X"));
        assert!(tmp.path().join("merge_journal/m-P1/samples/loser/P1.wav").exists());
        // 永久性失效(by=None,不可复活):样本副本同样保留——回执卡此时唯一能
        // 核对的就是这份快照,不该删。
        j.invalidate(&["P2"], "此人随后被改名", None);
        assert!(tmp.path().join("merge_journal/m-P2/samples/loser/P2.wav").exists());
    }

    #[test]
    fn revive_restores_entries_invalidated_by_given_id_only() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[], &[]).unwrap();
        j.append(&entry("m-P2", "t2", "P2", "P8"), &[], &[]).unwrap();
        j.invalidate(&["P9"], "相关人物随后又被合并", Some("m-X"));
        j.invalidate(&["P8"], "此人随后被改名", None);

        j.revive_invalidated_by("m-X");
        assert!(j.entry("m-P1").unwrap().invalid_reason.is_none(), "由 m-X 失效的复活");
        assert!(j.entry("m-P2").unwrap().invalid_reason.is_some(), "永久失效的不复活");
    }

    #[test]
    fn loser_sample_copies_lists_snapshot_or_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ls = fake_sample(&vpdir, "P1.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[ls], &[]).unwrap();
        assert_eq!(j.loser_sample_copies("m-P1").len(), 1);
        assert!(j.loser_sample_copies("../evil").is_empty());
        // 永久性失效后样本副本仍保留(唯一可核对的快照)
        j.invalidate(&["P1"], "此人随后被改名", None);
        assert_eq!(j.loser_sample_copies("m-P1").len(), 1);
    }

    #[test]
    fn loser_sample_copies_orders_by_slot_not_lexicographic() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        // 创建槽 1、2、10 的样本,字典序会错成 10, 2, 1
        let s1 = fake_sample(&vpdir, "P9.wav");
        let s2 = fake_sample(&vpdir, "P9-2.wav");
        let s10 = fake_sample(&vpdir, "P9-10.wav");
        j.append(&entry("m-P9", "t1", "P9", "P8"), &[s1, s2, s10], &[]).unwrap();

        let copies = j.loser_sample_copies("m-P9");
        assert_eq!(copies.len(), 3);
        // 按槽位序验证(1, 2, 10),而非字典序(1, 10, 2 或 10, 2, 1)
        assert_eq!(copies[0].file_name().unwrap(), "P9.wav", "槽 1 应在最前");
        assert_eq!(copies[1].file_name().unwrap(), "P9-2.wav", "槽 2 应在中间");
        assert_eq!(copies[2].file_name().unwrap(), "P9-10.wav", "槽 10 应在最后");
    }

    #[test]
    fn sample_copies_lists_winner_side_in_slot_order() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ws1 = fake_sample(&vpdir, "P2.wav");
        let ws2 = fake_sample(&vpdir, "P2-2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[], &[ws1, ws2]).unwrap();

        let copies = j.sample_copies("m-P1", "winner");
        assert_eq!(copies.len(), 2);
        assert_eq!(copies[0].file_name().unwrap(), "P2.wav", "槽 1 应在最前");
        assert_eq!(copies[1].file_name().unwrap(), "P2-2.wav", "槽 2 应在后");
    }

    #[test]
    fn write_loser_cut_sample_appears_after_numeric_slots() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let s1 = fake_sample(&vpdir, "P1.wav");
        let s2 = fake_sample(&vpdir, "P1-2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[s1, s2], &[]).unwrap();

        j.write_loser_cut_sample("m-P1", "P1", &[0.1f32; 100]);

        let copies = j.sample_copies("m-P1", "loser");
        assert_eq!(copies.len(), 3);
        assert_eq!(copies[0].file_name().unwrap(), "P1.wav");
        assert_eq!(copies[1].file_name().unwrap(), "P1-2.wav");
        assert_eq!(copies[2].file_name().unwrap(), "P1-cut.wav", "截声兜底文件排最后");
        assert!(copies[2].exists());
    }

    #[test]
    fn write_loser_cut_sample_illegal_id_does_not_panic_or_write() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.write_loser_cut_sample("../evil", "P1", &[0.1f32; 10]);
        assert!(j.sample_copies("../evil", "loser").is_empty());
        assert!(!tmp.path().join("merge_journal/../evil").exists(), "非法 id 不应落任何文件");
    }

    #[test]
    fn acknowledge_removes_entry_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[], &[]).unwrap();
        j.acknowledge("m-P1").unwrap();
        assert!(j.entries().is_empty());
        assert!(!tmp.path().join("merge_journal/m-P1").exists());
    }

    #[test]
    fn prune_caps_at_journal_cap_dropping_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        for i in 0..(JOURNAL_CAP + 3) {
            let id = format!("m-P{i}");
            let time = format!("2026-07-31T10:{:02}:{:02}+08:00", i / 60, i % 60);
            j.append(&entry(&id, &time, &format!("P{i}"), "W"), &[], &[]).unwrap();
        }
        let entries = j.entries();
        assert_eq!(entries.len(), JOURNAL_CAP);
        assert!(entries.iter().all(|e| e.id != "m-P0"), "最旧的被淘汰");
    }

    #[test]
    fn illegal_id_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entry("../evil").is_err());
        assert!(j.acknowledge("a/b").is_err());
    }

    #[test]
    fn denylist_roundtrip_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.auto_denylist().is_empty());
        j.deny_auto("P1>P2");
        j.deny_auto("P1>P2");
        j.deny_auto("P3>P4");
        assert_eq!(j.auto_denylist(), vec!["P1>P2".to_string(), "P3>P4".to_string()]);
    }

    #[test]
    fn dismissed_items_roundtrip_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.dismissed_items().is_empty());
        j.dismiss_item("s:P1>P2");
        j.dismiss_item("s:P1>P2");
        j.dismiss_item("d:张三");
        assert_eq!(j.dismissed_items(), vec!["s:P1>P2".to_string(), "d:张三".to_string()]);
    }

    #[test]
    fn dismissed_items_evicts_oldest_past_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        for i in 0..(DISMISSED_CAP + 1) {
            j.dismiss_item(&format!("n:P{i}"));
        }
        let list = j.dismissed_items();
        assert_eq!(list.len(), DISMISSED_CAP);
        assert!(!list.contains(&"n:P0".to_string()), "最旧的被淘汰");
        assert!(list.contains(&format!("n:P{DISMISSED_CAP}")), "最新的保留");
    }

    #[test]
    fn dismissed_items_corrupted_file_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        std::fs::create_dir_all(j.dir()).unwrap();
        std::fs::write(j.dismissed_path(), b"not json").unwrap();
        assert!(j.dismissed_items().is_empty());
    }

    #[test]
    fn restore_samples_copies_back_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ls = fake_sample(&vpdir, "P1.wav");
        let ws = fake_sample(&vpdir, "P2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[ls.clone()], &[ws.clone()]).unwrap();
        std::fs::remove_file(&ls).unwrap();
        std::fs::remove_file(&ws).unwrap();

        let n = j.restore_samples("m-P1", &vpdir).unwrap();
        assert_eq!(n, 2);
        assert!(ls.exists() && ws.exists());
    }

    #[test]
    fn restore_loser_samples_copies_only_loser_side() {
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P1", "loser", "P1.wav");
        put_side_file(&j, "m-P1", "winner", "P9.wav");
        let out = dir.path().join("vp");
        let n = j.restore_loser_samples("m-P1", &out).unwrap();
        assert_eq!(n, 1);
        assert!(out.join("P1.wav").exists());
        assert!(!out.join("P9.wav").exists(), "winner 侧不许被拷回");
    }

    #[test]
    fn restore_loser_samples_promotes_cut_to_slot1_when_no_regular() {
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P2", "loser", "P2-cut.wav");
        let out = dir.path().join("vp");
        let n = j.restore_loser_samples("m-P2", &out).unwrap();
        assert_eq!(n, 1);
        assert!(out.join("P2.wav").exists(), "仅有兜底截声时拷成槽 1,拆回的人才有样本可听");
        assert!(!out.join("P2-cut.wav").exists());
    }

    #[test]
    fn restore_loser_samples_ignores_cut_when_regular_slot_present() {
        // 常规槽位与兜底截声并存(常规槽是合并时真实迁移前的样本,截声只是兜底):
        // 只拷常规槽,截声不进正式库,避免同一人多出一份"降级"样本。
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P1", "loser", "P1.wav");
        put_side_file(&j, "m-P1", "loser", "P1-cut.wav");
        let out = dir.path().join("vp");
        let n = j.restore_loser_samples("m-P1", &out).unwrap();
        assert_eq!(n, 1, "常规槽位存在时只拷常规槽");
        assert!(out.join("P1.wav").exists());
        assert!(!out.join("P1-cut.wav").exists(), "截声不该被拷回");
    }

    #[test]
    fn restore_samples_never_overwrites_an_existing_sample() {
        // 拆回/撤销在样本恢复失败后保留 journal 让用户重试,而用户可能在重试前又录了
        // 新样本。恢复必须只补缺失:旧快照绝不能盖掉那份新录音
        // (codex review 实现轮二 P1)。
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P1", "loser", "P1.wav");
        put_side_file(&j, "m-P1", "winner", "P2.wav");
        let out = dir.path().join("vp");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("P1.wav"), b"RIFFbrand-new-recording").unwrap();

        let n = j.restore_samples("m-P1", &out).unwrap();
        assert_eq!(n, 1, "只该补缺失的 P2,P1 已存在应跳过且不计数");
        assert_eq!(
            std::fs::read(out.join("P1.wav")).unwrap(),
            b"RIFFbrand-new-recording",
            "既有样本必须原样保留"
        );
        assert!(out.join("P2.wav").exists());
        // 不留临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .filter(|f| f.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件必须清干净: {leftovers:?}");
    }

    #[test]
    fn restore_loser_samples_is_idempotent_and_keeps_new_recording() {
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P1", "loser", "P1.wav");
        let out = dir.path().join("vp");
        assert_eq!(j.restore_loser_samples("m-P1", &out).unwrap(), 1);
        std::fs::write(out.join("P1.wav"), b"RIFFbrand-new-recording").unwrap();
        // 重试:常规槽位已就位 → 拷 0 份,且**不得**退回去塞兜底截声。
        assert_eq!(j.restore_loser_samples("m-P1", &out).unwrap(), 0);
        assert_eq!(std::fs::read(out.join("P1.wav")).unwrap(), b"RIFFbrand-new-recording");
    }

    #[test]
    fn restore_loser_samples_still_falls_back_to_cut_on_retry_with_no_regular_slot() {
        // 兜底截声的判据是"根本没有常规槽位",不是"这次拷了 0 份"——否则重试时
        // 常规槽已就位(拷 0 份)会被误判成需要兜底。
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P2", "loser", "P2-cut.wav");
        let out = dir.path().join("vp");
        assert_eq!(j.restore_loser_samples("m-P2", &out).unwrap(), 1);
        assert!(out.join("P2.wav").exists());
        // 再来一次:目标已在,跳过,不重复也不报错。
        assert_eq!(j.restore_loser_samples("m-P2", &out).unwrap(), 0);
    }

    #[test]
    fn restore_samples_propagates_read_errors_instead_of_reporting_zero() {
        // 面向 UI 的 sample_copies 读不出来就当没有;恢复路径不能这样——上层会拿到
        // Ok(0) 就删掉 journal,唯一的样本副本永久丢失(codex review 实现轮二 P1)。
        let (dir, j) = test_journal();
        put_side_file(&j, "m-P1", "loser", "P1.wav");
        // 把 loser 侧目录换成一个普通文件:read_dir 报 NotADirectory,不是 NotFound。
        let side = j.samples_dir("m-P1", "loser").unwrap();
        std::fs::remove_dir_all(&side).unwrap();
        std::fs::write(&side, b"not a directory").unwrap();
        let out = dir.path().join("vp");
        assert!(j.restore_samples("m-P1", &out).is_err(), "读取失败必须上抛,不能报 Ok(0)");
        assert!(j.restore_loser_samples("m-P1", &out).is_err());
        // 宽松版仍然容错(它服务于 UI 列表)。
        assert!(j.sample_copies("m-P1", "loser").is_empty());
    }
}
