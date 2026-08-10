# 配置项大梳理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 声音处理方案默认 B;三删一藏(keep_audio/record_system_only/keep_output_volume 删,language_filter 藏);设置文件备份+容错;硬承诺双轨;AEC 逃生舱;容量治理;设置页分区重排+Segmented 统一+文案如实化。

**Architecture:** 后端 settings.rs 一次地基改造(键存在性迁移/备份/逐字段抢救/新枚举字段),lib.rs 消费点随后成批清理;硬承诺双轨改 `required_sources` 常量化+录音页授权引导;容量治理复用 `purge_audio(older_than_days)`;前端设置页按新分区重排,Segmented 承载全部 ≤3 互斥选项。

**Tech Stack:** Rust/serde、Svelte 5 runes、vitest、既有 `Segmented.svelte`。

**Spec:** `docs/superpowers/specs/2026-08-10-settings-overhaul-design.md`(v2,含 Codex 审查记录)

## Global Constraints

- **每个任务提交时全仓可编译、测试全绿**——删除类改动与其消费者清理必须同一提交。
- 迁移判定一律**原始键存在性**,禁止"值==默认值⇒未显式设置"(Codex P1#1)。
- settings.json 首次改写前必须有备份;解析失败逐字段抢救,禁止整对象静默重置(P1#2/#9)。
- 硬承诺双轨:System 升必备源,权限缺失/启动失败拒录+引导,不静默降级(P1#6,用户拍板)。
- AEC 逃生舱 `capture_path`(json-only 无 UI);录音页蓝牙/低音量提示改常态检测,文案不再指向已删开关(P1#3/4/5)。
- i18n zh/en 齐平过 parity;无硬编码 CJK;提交信息无 Co-Authored-By trailer;不跑全量 cargo fmt;禁 git stash。
- 分支 `settings-overhaul`(叠 audio-scheme-setting)。
- 后续项不做:麦克风设备选择器、Keychain(spec 已记)。

---

### Task 1: settings.rs 地基(加法)——键存在性迁移、默认 B、备份、抢救、新字段

**Files:**
- Modify: `src-tauri/src/settings.rs`

**Interfaces:**
- Produces:`AudioScheme` 默认 `B`;`Settings.audio_scheme` 对外仍具体值(内部经 `Option` 判存在性);新枚举 `CapturePath { Aec(默认), Vpio }` 字段 `capture_path`(serde `"aec"|"vpio"`)与 `AudioRetention { Forever(默认), D90, D30 }` 字段 `audio_retention`(serde `"forever"|"90d"|"30d"`);`load()` 内建备份与逐字段抢救。本任务**不删任何字段**。

- [ ] **Step 1: 写失败测试**(替换现有 audio_scheme 迁移测试组为新矩阵;沿用 tempfile 样板)

```rust
#[test]
fn audio_scheme_defaults_b_for_fresh_and_untouched_files() {
    // 全新安装(无文件)与旧默认文件(空对象)都落新默认 B
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);
    std::fs::write(tmp.path().join("settings.json"), "{}").unwrap();
    assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);
    assert_eq!(Settings::default().audio_scheme, AudioScheme::B, "unwrap 兜底口径");
}

#[test]
fn explicit_audio_scheme_always_wins_regardless_of_legacy() {
    // 键在场任意值照旧——含与陈旧 mix_track 并存(Codex P1#1 的翻车组合)
    for (raw, want) in [
        (r#"{"audio_scheme":"a"}"#, AudioScheme::A),
        (r#"{"audio_scheme":"b","mix_track":true}"#, AudioScheme::B),
        (r#"{"audio_scheme":"ab","mix_track":false}"#, AudioScheme::Ab),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), raw).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, want, "raw={raw}");
    }
}

#[test]
fn legacy_mix_track_migrates_only_when_new_key_absent() {
    for (raw, want) in [
        (r#"{"mix_track":true}"#, AudioScheme::Ab),
        (r#"{"mix_track":false}"#, AudioScheme::B), // 旧默认非显式选择,随新默认(用户拍板)
    ] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), raw).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, want, "raw={raw}");
    }
}

#[test]
fn load_backs_up_original_file_once_before_first_rewrite() {
    // 升级备份:load 发现旧格式(存在 mix_track 键或缺 settings_schema 标记)时拷贝一份,
    // 已有备份不覆盖(Codex P1#2:save 会立刻抹掉旧键,备份是唯一回退路径)
    let tmp = tempfile::tempdir().unwrap();
    let orig = r#"{"mix_track":true,"keep_audio":false}"#;
    std::fs::write(tmp.path().join("settings.json"), orig).unwrap();
    let _ = load(tmp.path());
    let bak = tmp.path().join("settings.json.bak-pre-overhaul");
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), orig, "备份=原文");
    // 二次 load(备份已存在)不得覆盖
    std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"a"}"#).unwrap();
    let _ = load(tmp.path());
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), orig, "备份不被后续覆盖");
}

#[test]
fn corrupt_file_is_salvaged_field_by_field_not_reset() {
    // 单字段类型错不再拖垮整对象(Codex P1#9):坏字段用默认,好字段保留
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("settings.json"),
        r#"{"audio_scheme":123,"dashscope_api_key":"sk-live","theme":"dark"}"#,
    ).unwrap();
    let s = load(tmp.path());
    assert_eq!(s.audio_scheme, AudioScheme::B, "坏字段回默认");
    assert_eq!(s.dashscope_api_key, "sk-live", "好字段(凭证)不得丢");
    assert_eq!(s.theme, "dark");
    // 整文件截断:抢救不出任何字段 → 默认,但坏文件要留尸检
    std::fs::write(tmp.path().join("settings.json"), r#"{"broken"#).unwrap();
    let s = load(tmp.path());
    assert_eq!(s.audio_scheme, AudioScheme::B);
    let corpses: Vec<_> = std::fs::read_dir(tmp.path()).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("settings.json.corrupt-"))
        .collect();
    assert!(!corpses.is_empty(), "截断文件须备份为 corrupt-*");
}

#[test]
fn capture_path_and_retention_default_and_parse() {
    let s: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(s.capture_path, CapturePath::Aec);
    assert_eq!(s.audio_retention, AudioRetention::Forever);
    let s: Settings = serde_json::from_str(r#"{"capture_path":"vpio","audio_retention":"30d"}"#).unwrap();
    assert_eq!(s.capture_path, CapturePath::Vpio);
    assert_eq!(s.audio_retention, AudioRetention::D30);
}
```

- [ ] **Step 2: RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml settings`
Expected: 编译错误(新枚举/新语义不存在)。

- [ ] **Step 3: 实现**

要点(结构照抄现有风格,注释写为什么):

```rust
// 字段区:
/// 声音处理方案。默认 B(2026-08-10 用户拍板:成品轨为旗舰默认)。
/// 内部 Option 仅为判"键是否在场"(迁移必须看原始键存在性,不能比较值==默认——
/// 默认翻转后显式 "b" 会被误判未设置,Codex P1#1);resolve 后对外恒具体值。
#[serde(default, skip_serializing_if = "Option::is_none", rename = "audio_scheme")]
audio_scheme_raw: Option<AudioScheme>,   // 私有,serde 专用
#[serde(skip)]
pub audio_scheme: AudioScheme,           // resolve 后的对外真值,save 前回写 raw
/// 采集路径逃生舱(json-only 无 UI,同 asr_provider 先例):aec=普通输入+软件AEC(默认),
/// vpio=系统通话模式(蓝牙击穿/设备格式不兼容时的手改退路)。
#[serde(default)]
pub capture_path: CapturePath,
/// 音频自动保留期:到期笔记仅清音频轨(转写/精修稿永留)。默认永久。
#[serde(default)]
pub audio_retention: AudioRetention,
```

注意:`audio_scheme` 的 skip 字段要在 `save` 前把真值写回 `audio_scheme_raw = Some(self.audio_scheme)`(在 `save()` 入口做,或提供 `fn sync_raw(&mut self)`),否则写盘丢键。枚举:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapturePath { #[default] Aec, Vpio }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioRetention {
    #[default]
    #[serde(rename = "forever")] Forever,
    #[serde(rename = "90d")] D90,
    #[serde(rename = "30d")] D30,
}
impl AudioRetention {
    /// 保留天数;Forever = None。
    pub fn days(self) -> Option<u32> { match self { Self::Forever => None, Self::D90 => Some(90), Self::D30 => Some(30) } }
}
```

`load()` 重写(备份→解析/抢救→迁移 resolve):

```rust
pub fn load(app_data: &Path) -> Settings {
    let path = app_data.join("settings.json");
    let raw = std::fs::read_to_string(&path).ok();
    // 升级备份:旧格式文件(含任一旧键)首次见到即整文件备份,已有不覆盖。
    if let Some(text) = &raw {
        let looks_legacy = ["\"mix_track\"", "\"keep_audio\"", "\"record_system_only\"", "\"keep_output_volume\"", "\"mirror_prefix\""]
            .iter().any(|k| text.contains(k));
        let bak = app_data.join("settings.json.bak-pre-overhaul");
        if looks_legacy && !bak.exists() {
            let _ = std::fs::write(&bak, text);
        }
    }
    let mut s: Settings = match raw.as_deref().map(serde_json::from_str::<Settings>) {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            // 整对象失败 → 尸检备份 + 逐字段抢救(Codex P1#9:静默整体重置会连凭证一起丢)
            eprintln!("settings.json 解析失败,逐字段抢救: {e}");
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let _ = std::fs::write(app_data.join(format!("settings.json.corrupt-{ts}")), raw.as_deref().unwrap_or(""));
            salvage(raw.as_deref().unwrap_or(""))
        }
        None => Settings::default(),
    };
    // 迁移 resolve:原始键在场则照旧;缺失时看旧 mix_track;都缺 → 默认 B。
    s.audio_scheme = match s.audio_scheme_raw {
        Some(v) => v,
        None => match s.legacy_mix_track {
            Some(true) => AudioScheme::Ab,
            _ => AudioScheme::B,
        },
    };
    s
}
```

**执行修订:此伪码的贪心剔除法被实现证伪(会把好字段连凭证一起重置为默认——settings.rs::salvage 的注释有论证),落地实现为增量叠加法,以代码为准。**

```rust
/// 逐字段抢救:整体 JSON 不合法或字段类型错时,能从 Value 读出的字段保留,读不出的用默认。
fn salvage(text: &str) -> Settings {
    let mut s = Settings::default();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return s };
    let Some(obj) = v.as_object() else { return s };
    // 把默认 Settings 序列化为 Value,逐键用文件里"能按目标类型反序列化"的值覆盖,再整体反序列化。
    let mut base = serde_json::to_value(&s).unwrap_or(serde_json::Value::Object(Default::default()));
    if let Some(base_obj) = base.as_object_mut() {
        for (k, val) in obj {
            base_obj.insert(k.clone(), val.clone());
        }
    }
    // 先整体试;失败则逐键回退(把与默认同键但类型不合的项剔除后重试)
    match serde_json::from_value::<Settings>(base.clone()) {
        Ok(ok) => s = ok,
        Err(_) => {
            if let (Some(base_obj), Ok(default_v)) = (base.as_object_mut(), serde_json::to_value(Settings::default())) {
                let default_obj = default_v.as_object().cloned().unwrap_or_default();
                let keys: Vec<String> = base_obj.keys().cloned().collect();
                for k in keys {
                    let mut probe = base.clone();
                    // 尝试剔除该键(回落默认值),若可解析则说明该键是坏源
                    if let Some(po) = probe.as_object_mut() {
                        match default_obj.get(&k) {
                            Some(dv) => { po.insert(k.clone(), dv.clone()); }
                            None => { po.remove(&k); }
                        }
                    }
                    if let Ok(ok) = serde_json::from_value::<Settings>(probe.clone()) {
                        s = ok; base = probe;
                    }
                }
            }
        }
    }
    s
}
```

(salvage 的逐键剔除是 O(n²) 但 n≈30 且仅坏文件路径,可接受;实现者可优化但语义必须过 Step 1 测试。)`save()` 入口加 `let mut s2 = s.clone(); s2.audio_scheme_raw = Some(s2.audio_scheme);` 后序列化 s2(或等价手段);`update()` 闭包后同样经 load→改→save 链天然覆盖。`Default` impl:`audio_scheme_raw: None, audio_scheme: AudioScheme::B, capture_path/audio_retention` 默认。旧 `legacy_mix_track` 字段保留(本任务不删)。前一单的迁移测试(`explicit_new_key_wins_over_legacy_mix_track` 等)按新矩阵改写/吸收。

- [ ] **Step 4: GREEN + 全量**

Run: `cargo test --manifest-path src-tauri/Cargo.toml settings && cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 全绿,无新警告(lib.rs 尚未消费新字段,dead_code 警告若出现,在字段上临时 `#[allow(dead_code)]` 并注明 Task N 接线后删——或本任务即接 capture_path 消费,见 Task 2 拆分说明,允许实现者与控制器商定归属)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/settings.rs
git commit -m "feat(settings): 键存在性迁移+默认 B+升级备份+逐字段抢救+capture_path/audio_retention 字段"
```

---

### Task 2: 后端删除与消费清理(同一提交保持绿)

**Files:**
- Modify: `src-tauri/src/settings.rs`(删三字段+mirror_prefix+其迁移与测试)
- Modify: `src-tauri/src/lib.rs`(全部消费点)
- Modify: `src-tauri/src/telemetry.rs`

**Interfaces:**
- Consumes:Task 1 的 `capture_path`。
- Produces:`Settings` 不再含 `keep_audio/record_system_only/keep_output_volume/mirror_prefix`;`RecordSource::from_settings` 删除。

- [ ] **Step 1: settings.rs 删除**

删字段 `keep_audio/record_system_only/keep_output_volume/mirror_prefix` 及 Default 项、`migrate_mirror_prefix()` 及其测试、三字段相关默认值测试;镜像 URL 改编译期常量(`pub const MIRROR_PREFIX: &str = ...`,值取原默认)。补测试:

```rust
#[test]
fn deleted_legacy_keys_still_parse_and_are_dropped_on_save() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("settings.json"),
        r#"{"keep_audio":false,"record_system_only":true,"keep_output_volume":true,"mirror_prefix":"https://x/"}"#).unwrap();
    let s = load(tmp.path());
    save(tmp.path(), &s).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
    for k in ["keep_audio", "record_system_only", "keep_output_volume", "mirror_prefix"] {
        assert!(!raw.contains(k), "旧键 {k} 不得复活: {raw}");
    }
    // 且备份已存在(Task 1 的 looks_legacy 覆盖这些键)
    assert!(tmp.path().join("settings.json.bak-pre-overhaul").exists());
}
```

- [ ] **Step 2: lib.rs 消费清理(逐处,grep 驱动)**

先 `grep -n "keep_audio\|record_system_only\|keep_output_volume\|mirror_prefix\|migrate_mirror_prefix\|from_settings" src-tauri/src/lib.rs src-tauri/src/telemetry.rs src-tauri/src/models/*.rs` 列全清单,然后:

1. 快照元组(~L984-996):删三项,`keep_output_volume` 位置改取 `let use_aec_capture = cfg.capture_path == settings::CapturePath::Aec;`。
2. sink 装配(~L1445-1460):`keep_audio` 分支删除,恒走装配(else 空臂删)。
3. AEC 条件(~L1657):`(keep_output_volume || cfg!(windows))` → `(use_aec_capture || cfg!(windows))`;mic 采集 VPIO/普通输入选择处同改(grep `keep_output_volume` 找到的采集入口)。
4. 源构建:`record_system_only` 相关分支删除——恒建 mic+system 两源(System 构建失败分支保留但其后果由 Task 3 的必备源守卫接管,本任务先保持"失败打日志"现状,**不删失败分支**)。
5. `required_sources(system_only)` 参数暂留(Task 3 改),调用点传 `false`。
6. telemetry.rs:`RecordSource::from_settings` 删除,两个调用点(lib.rs:~1802/1871 区域)直接用 `telemetry::RecordSource::Both`;`required_sources_follow_system_only` 测试改为断言默认必备源(Task 3 会再改,此处仅保编译绿)。
7. 启动处 `migrate_mirror_prefix` 调用改为普通 `settings::load` + save(若该调用只为迁移,直接删调用链);镜像消费(models 下载)改读常量。

- [ ] **Step 3: 验证**

Run: `cargo test --manifest-path src-tauri/Cargo.toml settings && cargo test --manifest-path src-tauri/Cargo.toml required_sources && cargo check --manifest-path src-tauri/Cargo.toml && cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -5`
Expected: 全量测试绿(含 lib.rs:7140 续录同步测试——其 keep_audio=false 契约按 spec §3 改写为"writer 未写入"场景;lib.rs:7221 required_sources 测试同步调整)。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/settings.rs src-tauri/src/lib.rs src-tauri/src/telemetry.rs
git commit -m "feat(settings): 三删一常量——keep_audio/system_only/keep_output_volume/mirror_prefix 出局,AEC 走 capture_path"
```

---

### Task 3: 硬承诺双轨——必备源+拒录引导

**Files:**
- Modify: `src-tauri/src/lib.rs`(`required_sources` 常量化、System 构建失败硬化)
- Modify: `src/routes/record/+page.svelte` + `src/lib/i18n/dict/record.ts`(拒录引导卡)

**Interfaces:**
- Produces:开录失败错误串包含稳定前缀 `system_denied` / `system_unavailable`(前端据此分支);录音页授权引导卡。

- [ ] **Step 1: 后端**

`required_sources()` 去参数恒返 `vec![Source::Mic, Source::System]`(纯函数测试同步改写:两源皆必备)。System VAD 构建失败分支(~L812 "降级为仅麦克风" eprintln)改为把失败记入 `failed` 集合走既有 Fix A 拆除路径——错误分类沿用现有 `denied`/`unavailable` 判定(lib.rs:805-810),错误文案里带上该分类词,前端可判。**读现有 Fix A 拆除与 fail 文案构造代码后再动手**;行为目标:System 起不来 → 整场拆除 + 错误消息标明是权限还是设备问题。

- [ ] **Step 2: 前端引导卡**

record 页开录失败处理处(找现有 error 展示分支):错误串含 `denied` 时渲染引导卡(结构照设置页日历说明卡先例:说明文案 + 「打开系统设置」按钮)。打开系统设置走新后端命令:

```rust
/// 打开系统设置的屏幕录制隐私页(硬承诺双轨的授权引导)。opener 同 open_models_dir 先例。
#[tauri::command]
fn open_screen_capture_settings(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture", None::<&str>)
        .map_err(|e| tr!("打开系统设置失败: {e}", "Failed to open System Settings: {e}"))
}
```

注册 generate_handler;models.ts 加包装。i18n 新 key(record 域,zh/en):`record.systemDenied.title/desc/openSettings`、`record.systemUnavailable.desc`(文案:会议笔记需要录制系统声音;请在 系统设置→隐私与安全性→屏幕录制 中允许本应用,然后重试)。Windows 侧(`#[cfg(windows)]`)该 URL 无效——命令做平台分支(Windows 返回 Err 提示暂不支持自动跳转,文案兜底);Windows 采集本无此权限模型,引导卡按 `unavailable` 文案走。

- [ ] **Step 3: 验证 + Commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml required_sources && cargo check --manifest-path src-tauri/Cargo.toml && npm run check && npm test`

```bash
git add src-tauri/src/lib.rs "src/routes/record/+page.svelte" src/lib/i18n/dict/record.ts src/lib/models.ts
git commit -m "feat(record): 硬承诺双轨——System 升必备源,权限缺失拒录+屏幕录制授权引导卡"
```

---

### Task 4: 容量治理——音频自动保留期

**Files:**
- Modify: `src-tauri/src/lib.rs`(启动清理任务)
- Test: settings.rs 已有 `AudioRetention::days()`;lib.rs 侧判定抽纯函数配测试

**Interfaces:**
- Consumes:Task 1 `audio_retention` 与既有 `purge_audio` 内部逻辑(lib.rs:6409——`older_than_days: Option<u32>`,cutoff 计算与逐笔记清音频轨)。
- Produces:启动后台执行一次自动清理(retention != Forever 时)。

- [ ] **Step 1: 实现**

把 `purge_audio` 命令的清理本体抽成内部函数 `purge_audio_older_than(app: &AppHandle, days: u32) -> Result<u64, String>`(命令与自动清理共用,防两处漂移)。启动流程(setup 完成后,与其它后台任务同区——找现有 `tauri::async_runtime::spawn` 先例)追加:

(执行修订:共用体签名保留 Option<u32>,None=全清是命令的既有生产语义,brief 的裸 u32 会逼出第二份循环。)

```rust
// 音频自动保留期(spec §5):到期笔记仅清音频轨,转写/精修稿永留。启动后台跑一次,
// 失败仅打日志(容量治理是增值层,绝不挡启动)。录制中笔记天然不在清理范围
// (复用 purge_audio 既有的活动笔记豁免——实现前先读原命令确认该豁免存在,无则补)。
if let Some(days) = cfg.audio_retention.days() {
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        match purge_audio_older_than(&app2, days) {
            Ok(freed) => eprintln!("音频保留期清理完成: 释放 {freed} 字节(>{days} 天)"),
            Err(e) => eprintln!("音频保留期清理失败(不影响使用): {e}"),
        }
    });
}
```

活动/录制中笔记豁免必须核实;`purge_audio` 原命令签名与前端调用不变。

- [ ] **Step 2: 验证 + Commit**

Run: `cargo check --manifest-path src-tauri/Cargo.toml && cargo test --manifest-path src-tauri/Cargo.toml purge`

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(store): 音频自动保留期——启动后台清理到期音频轨,命令与自动路径共用一体"
```

---

### Task 5: 前端类型 + 设置页大重排

**Files:**
- Modify: `src/lib/models.ts`(类型:删三字段+mirror_prefix,加 `capture_path`/`audio_retention`;`openScreenCaptureSettings` 已在 Task 3)
- Modify: `src/routes/settings/+page.svelte`
- Modify: `src/lib/i18n/dict/settings.ts`

**Interfaces:**
- Consumes:Segmented 组件;后端新 serde 键。
- Produces:新分区结构(通用/录制/存储/语音模型/高级折叠/关于)。

- [ ] **Step 1: models.ts 类型同步**(删 `keep_audio/record_system_only/keep_output_volume/mirror_prefix`,加 `capture_path: "aec" | "vpio"; audio_retention: "forever" | "90d" | "30d";`——capture_path 无 UI 但类型齐全防 set_settings 丢键)

- [ ] **Step 2: 设置页重排**(逐项,全部沿用既有 saveSetting 纪律)

1. 删三行(keep_audio/record_system_only/keep_output_volume)及其 state/回填。
2. ≤3 互斥选项换 Segmented:主题(system/light/dark)、界面语言(system/zh/en)、识别方式(local/cloud)、声纹模型(campplus/eres2netv2)、云厂商(volcano/aliyun)、音频保留期(forever/90d/30d,新行,放「存储」区磁盘占用/清理旁)。既有 select/radio 标记删除;每处 `items` derived + `disabled: !settings` 纪律同 audioScheme 先例。
3. 新「高级」折叠区(置于「关于」前):`<button class="adv-toggle" aria-expanded={advOpen}>` + chevron 旋转 + 120ms max-height/opacity 过渡(reduced-motion 直切);内含 语言过滤 行与 identify_auto_apply 行(原样移入);展开态为组件 $state(页面挂载内记忆,如实注释)。
4. 「存储」区保留可见:数据/模型目录 + 磁盘占用 + 手动清理 + 新保留期行。
5. mirror 行只留开关(自定义前缀 UI 若存在则删)。
6. i18n:删除行的 key 移除;新增 `settings.section.advanced`、`settings.record.audioRetention.*`(label/desc/三段名)、高级区说明;zh/en 齐平。

- [ ] **Step 3: 验证 + Commit**

Run: `npm run check && npm test`
Expected: 0/0 + 全绿(parity/noHardcodedCjk 哨兵)。

```bash
git add src/lib/models.ts "src/routes/settings/+page.svelte" src/lib/i18n/dict/settings.ts
git commit -m "feat(settings): 设置页大重排——新分区/高级折叠/Segmented 全统一/保留期入存储区"
```

---

### Task 6: 录音页常态检测 + 文案/状态如实化

**Files:**
- Modify: `src/routes/record/+page.svelte`(蓝牙/低音量提示去开关依赖)
- Modify: `src/lib/i18n/dict/record.ts`(蓝牙文案重写)
- Modify: `src/routes/settings/+page.svelte` + `src/lib/i18n/dict/settings.ts`(lockHint 分层、AI 就绪状态)

**Interfaces:**
- Consumes:Task 5 后的设置页结构。

- [ ] **Step 1: 录音页**(record/+page.svelte:62/78 一带)——两处提示的 `keep_output_volume`/相关设置读取删除,改恒按设备状态判定(蓝牙输出→回声风险提示;输入音量低→低音量提示)。蓝牙提示文案(record.ts:34/89 一带)重写:不再指向已删开关,改为"蓝牙耳机延迟可能影响回声消除效果,建议改用有线耳机或内置扬声器"(zh/en)。

- [ ] **Step 2: lockHint 分层**——settings.ts 的 `settings.lockHint.*` 改三句:识别方式录制中锁定;主题/托盘/界面语言即时生效;其余下一场录制生效。设置页渲染处按现结构改文案 key 即可(若一行放不下拆两行 hint)。

- [ ] **Step 3: AI 就绪状态**——设置页 refine_enabled 行:开关不动,旁挂状态徽标——`refineOn && !refineReady` 时显示"未配置完成"(warning-ink 小字)+ `goto("/ai")` 链接。`refineReady` 判定:base_url/model/api_key(openai 档)或 agent 档对应字段非空——从已回填的 settings 派生,口径对齐后端 readiness(lib.rs:262 一带,实现前先读)。i18n 新 key `settings.refine.notReady`、`settings.refine.goConfig`。

- [ ] **Step 4: 验证 + Commit**

Run: `npm run check && npm test`

```bash
git add "src/routes/record/+page.svelte" src/lib/i18n/dict/record.ts "src/routes/settings/+page.svelte" src/lib/i18n/dict/settings.ts
git commit -m "feat(ui): 录音提示常态化+lockHint 分层+AI 就绪状态如实化——文案不再指向已删开关"
```

---

### Task 7: 收口——死键清扫 + 全量验证

**Files:**
- 全仓 grep 清扫,无预期新增文件

- [ ] **Step 1: 死引用扫**

`grep -rn "keep_audio\|record_system_only\|keep_output_volume\|mirror_prefix\|mixTrack\|from_settings" src src-tauri/src --include="*.rs" --include="*.ts" --include="*.svelte"` ——白名单:settings.rs 的迁移/兼容/备份测试里的原始 JSON 字符串与 `legacy_mix_track` 字段本体;其余命中皆为死引用,清掉。

- [ ] **Step 2: 全量验证**

Run: `npm run check && npm test && cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -5 && cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 全绿。

- [ ] **Step 3: Commit(如有清扫改动)**

```bash
git add -u && git commit -m "chore(settings): 大梳理收口——死键清扫"
```

---

### 收尾(不在任务内,提醒执行者)

- 真机冒烟(用户做,写进 PR 描述):①三种旧文件升级(无键→B/true→AB/false→B)+备份文件生成;②AEC 常态录一段回声表现;③蓝牙耳机提示出现且文案无死指向;④a 屏幕录制权限被拒(denied)时拒录+引导卡+跳系统设置;④b 屏幕录制权限不可用(unavailable,非用户手动拒绝,如系统不支持/虚拟机环境)时拒录卡文案须含诊断 hint,不能与④a 共用同一句笼统文案;⑤保留期 30d 设定后启动清理生效且录制中笔记豁免;⑥逃生舱手改 capture_path=vpio 生效;⑦设置页六分区/高级折叠/全部 Segmented 双主题;⑧AI 未配置完成徽标与跳转;⑨损坏 settings.json 启动不丢凭证;⑩lockHint 三层文案抽验(识别方式录制中锁定/主题托盘语言即时生效/其余下一场生效三档文案),分别在**录制中**与**空闲**两态下人眼核对措辞与实际生效时机一致,不再一刀切;⑪设置保留期(如 30d)后重启应用,在启动清理任务运行窗口内点开录应被拒绝,窗口结束后无需额外操作即可自动恢复可录(见下方风险台账已知小坑)。
- PR 叠 #85;提交无 Co-Authored-By。
- 风险台账(spec §2/§4):AEC 固定与硬承诺双轨是行为变更之最,冒烟翻车回退 = capture_path 逃生舱 / 恢复降级分支,均有备份文件兜底。
- 风险台账(已知小坑,冒烟⑪期间会看到):启动清理复用 `migrate_guard` 的 `download_running` 互斥位,清理窗口内点开录被拒时前端展示的是"正在迁移或下载,稍后再试"(`Migration or download in progress; try again later`)——此刻实际在跑的是音频保留期清理,不是迁移也不是下载,文案对用户是误导性的;窗口很短且会自动放行,不阻塞使用,暂不改文案,先记台账。
