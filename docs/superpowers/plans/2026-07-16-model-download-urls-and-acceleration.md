# 模型下载:地址显示 + 多代理加速 + 并发下载 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 设置页展示每个语音模型的下载地址,把镜像加速升级为多代理自动回退,并把串行下载改为有上限的并发下载。

**Architecture:** 纯增量,不改模型加载/解压/校验链。后端 `ArtifactState` DTO 多吐 `url`;`download_urls` 从「镜像→原站」两级扩为「主代理→备用代理→原站」多级回退;`download_models` 的串行循环改为 `std::thread::scope` 有界并发 + 失败隔离;`settings.rs` 换默认前缀并一次性迁移存量旧默认。前端模型行改为可展开显示地址。

**Tech Stack:** Rust / Tauri v2(后端命令 + 事件),Svelte 5 runes(`$state`),ureq(下载,已有)。仅用 std 并发(`thread::scope` / `AtomicUsize` / `AtomicBool`),不引新依赖。

## Global Constraints

- 不接 ModelScope;不改模型加载、tar.bz2 解压、sha256 校验逻辑。
- 不给镜像前缀加 UI 编辑框(产品决策:只改默认值);健壮性靠多代理回退。
- 提交信息正文直接结尾,**不加** `Co-Authored-By` / “Generated with Claude” 等任何署名尾注。
- 默认主代理前缀:`https://ghfast.top/`(注意结尾斜杠)。旧默认:`https://ghproxy.net/`。
- 并发上限常量 `MAX_CONCURRENT_DOWNLOADS = 3`;每 URL 重试常量 `DOWNLOAD_ATTEMPTS_PER_URL = 3`(已存在,lib.rs:33)。
- DTLN release `models-dtln-aec-v1` 已于本日修复发布,本计划**不含**该带外修复;仅在注册处补维护注释。
- 网络路径(实际下载)按本仓惯例**不做单测**,靠人工冒烟;纯函数(`download_urls`/迁移/DTO 映射)必须单测。

---

## 文件结构

- `src-tauri/src/models/mod.rs` —— `ArtifactState` 加 `url` 字段;`status()` 映射填 `url`;DTLN 注册处补注释;新增 DTO url 单测。
- `src-tauri/src/models/download.rs` —— 新增备用代理常量 `BACKUP_MIRROR_PREFIXES`;重写 `download_urls` 为多代理回退;更新/新增单测。
- `src-tauri/src/settings.rs` —— `DEFAULT_MIRROR_PREFIX` 改 ghfast.top;新增 `LEGACY_MIRROR_PREFIX` 与 `migrate_mirror_prefix()`;新增迁移单测。
- `src-tauri/src/lib.rs` —— 新增 `MAX_CONCURRENT_DOWNLOADS` 常量与 `download_one()` 辅助函数;重写 `download_models` 下载段为并发 + 失败隔离;setup 中调用一次迁移。
- `src/lib/models.ts` —— `ArtifactState` 类型加 `url: string`。
- `src/routes/settings/+page.svelte` —— 模型行可展开显示原始/镜像地址;新增 `effectiveUrl` 前端拼接函数。

依赖顺序:Task 1 → Task 5(前端消费 url);Task 2 → Task 4(并发下载消费多代理回退)。Task 3 独立。建议按 1→2→3→4→5 执行。

---

### Task 1: 后端 `ArtifactState` 暴露 `url`(+ 前端类型 + DTLN 注释)

**Files:**
- Modify: `src-tauri/src/models/mod.rs:256-262`(struct)、`:277-284`(映射)、`:201-226`(DTLN 注释)
- Modify: `src/lib/models.ts:4-10`(类型)
- Test: `src-tauri/src/models/mod.rs`(内联 `#[cfg(test)]`)

**Interfaces:**
- Produces:`ArtifactState { id, label, approx_mb, required_for_recording, present, url }`(`url: String`,值 = `Artifact.url`)。前端 `ModelsStatus.artifacts[i].url` 供 Task 5 消费。

- [ ] **Step 1: 写失败测试** —— 在 `src-tauri/src/models/mod.rs` 的 `mod tests` 内加:

```rust
    #[test]
    fn status_exposes_artifact_urls() {
        let st = status("sense_voice");
        assert_eq!(st.artifacts.len(), ARTIFACTS.len());
        for s in &st.artifacts {
            let a = ARTIFACTS.iter().find(|a| a.id == s.id).expect("id 应在注册表");
            assert_eq!(s.url, a.url, "DTO url 应等于注册表 url");
            assert!(!s.url.is_empty(), "url 不应为空");
        }
    }
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test -p voice-notes status_exposes_artifact_urls 2>&1 | tail -20`
Expected: 编译错误 `no field 'url' on type '&ArtifactState'`(字段尚未加)。

- [ ] **Step 3: 加 `url` 字段** —— `src-tauri/src/models/mod.rs`,在 struct 末尾(`present` 后)加字段:

```rust
pub struct ArtifactState {
    pub id: String,
    pub label: String,
    pub approx_mb: u64,
    pub required_for_recording: bool,
    pub present: bool,
    /// 该工件的原始下载地址(GitHub release 直链),供设置页展示。
    pub url: String,
}
```

- [ ] **Step 4: 映射填 `url`** —— 同文件 `status()` 的 `.map(|a| ArtifactState { ... })`,在 `present:` 后加一行:

```rust
        .map(|a| ArtifactState {
            id: a.id.into(),
            label: a.label.into(),
            approx_mb: a.approx_mb,
            required_for_recording: required_now(a.id, asr_model),
            present: artifact_present(&root, a),
            url: a.url.into(),
        })
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test -p voice-notes status_exposes_artifact_urls 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 6: DTLN 注册处补维护注释** —— 同文件,在 `id: "dtln_aec_256_1"` 的 `Artifact {` 上方注释块内补一句(紧接现有 `// DTLN-aec 256 档...` 注释):

```rust
    // 维护提醒:这两个 onnx 靠手动发布的 public GitHub release(tag models-dtln-aec-v1)
    // 分发,全网无官方 onnx 源。今后更新模型或改 tag,必须同步发布对应 public release 并
    // 上传资产,否则匿名用户下载 404(曾因 release 从未发布导致全体用户下不了)。
```

- [ ] **Step 7: 前端类型加 `url`** —— `src/lib/models.ts:4-10`:

```typescript
export type ArtifactState = {
  id: string;
  label: string;
  approx_mb: number;
  required_for_recording: boolean;
  present: boolean;
  /** 原始下载地址(GitHub release 直链),设置页展开展示用。 */
  url: string;
};
```

- [ ] **Step 8: 提交**

```bash
git add src-tauri/src/models/mod.rs src/lib/models.ts
git commit -m "feat(models): ArtifactState 暴露下载 url + DTLN 发布维护注释"
```

---

### Task 2: `download_urls` 多代理自动回退

**Files:**
- Modify: `src-tauri/src/models/download.rs:26-33`(`download_urls`)、新增常量
- Test: `src-tauri/src/models/download.rs:362-371`(更新)+ 新增用例

**Interfaces:**
- Consumes:已有 `apply_mirror(url, enabled, prefix) -> String`(不改)。
- Produces:`download_urls(url, mirror_enabled, mirror_prefix) -> Vec<String>`,启用时 = `[主代理+url, 备用代理+url(去重), …, 原站url]`,原站恒为最后一项;停用/空前缀 = `[原站url]`。Task 4 依据「原站是最后一项」区分重试次数。

- [ ] **Step 1: 更新 + 新增失败测试** —— `src-tauri/src/models/download.rs` 的 `mod tests`。先把现有 `download_urls_try_mirror_first_then_origin_when_enabled`(362-371)整体替换为下面这版,并在其后新增两个用例:

```rust
    #[test]
    fn download_urls_try_mirror_first_then_origin_when_enabled() {
        let u = "https://github.com/a/b.onnx";
        assert_eq!(download_urls(u, false, "https://ghproxy.net/"), vec![u.to_string()]);
        assert_eq!(download_urls(u, true, ""), vec![u.to_string()], "空前缀视同停用");
        let urls = download_urls(u, true, "https://ghproxy.net/");
        assert_eq!(urls.first().unwrap(), &format!("https://ghproxy.net/{u}"), "主代理在最前");
        assert_eq!(urls.last().unwrap(), u, "原站恒为最后");
    }

    #[test]
    fn download_urls_multi_proxy_dedup_and_origin_last() {
        let u = "https://github.com/a/b.onnx";
        // 主代理恰好等于某个备用代理时,该代理只应出现一次。
        let backup0 = format!("{}{u}", BACKUP_MIRROR_PREFIXES[0]);
        let urls = download_urls(u, true, BACKUP_MIRROR_PREFIXES[0]);
        assert_eq!(urls.iter().filter(|x| **x == backup0).count(), 1, "主/备重复应去重");
        assert_eq!(urls.last().unwrap(), u, "原站恒为最后");
        // 全程无重复项。
        let mut sorted = urls.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), urls.len(), "候选列表不应有重复");
    }

    #[test]
    fn download_urls_includes_backup_proxies() {
        let u = "https://github.com/a/b.onnx";
        let urls = download_urls(u, true, "https://ghfast.top/");
        // 主代理 + 至少一个备用代理 + 原站。
        assert!(urls.len() >= 3, "应含主代理、备用代理与原站, got {urls:?}");
        for bp in BACKUP_MIRROR_PREFIXES {
            if *bp != "https://ghfast.top/" {
                assert!(urls.iter().any(|x| x == &format!("{bp}{u}")), "应含备用代理 {bp}");
            }
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test -p voice-notes download_urls 2>&1 | tail -25`
Expected: 编译失败 `cannot find value 'BACKUP_MIRROR_PREFIXES'`,以及 `download_urls_includes_backup_proxies` 逻辑失败(现实现只回两项)。

- [ ] **Step 3: 加常量 + 重写 `download_urls`** —— `src-tauri/src/models/download.rs`,在 `apply_mirror`(14-24)与 `download_urls`(26-33)之间加常量,并替换 `download_urls`:

```rust
/// 备用 GitHub 下载代理(主代理失败后按序回退)。均为「前缀 + 完整原始 URL」的 ghproxy 风格。
/// 存活实测(2026-07-16)健在;公共代理会波动,失效时改此列表并发版。列表短以压回退延迟。
pub const BACKUP_MIRROR_PREFIXES: &[&str] = &["https://gh-proxy.com/", "https://ghproxy.net/"];

/// 下载候选 URL(按序尝试):启用镜像时 = [主代理+url, 备用代理+url(去重), 原站url];
/// 停用/空前缀 = [原站url]。原站恒为最后一项。Task 4 依此区分「代理少重试、原站多重试」。
pub fn download_urls(url: &str, mirror_enabled: bool, mirror_prefix: &str) -> Vec<String> {
    let primary = apply_mirror(url, mirror_enabled, mirror_prefix);
    if primary == url {
        // 镜像停用或空前缀:只有原站。
        return vec![url.to_string()];
    }
    let mut out = vec![primary];
    for bp in BACKUP_MIRROR_PREFIXES {
        let candidate = apply_mirror(url, true, bp);
        if candidate != url && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out.push(url.to_string()); // 原站兜底,恒最后
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test -p voice-notes download_urls 2>&1 | tail -25`
Expected: 三个 `download_urls_*` 用例全 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/models/download.rs
git commit -m "feat(download): download_urls 多代理自动回退(主→备→原站)"
```

---

### Task 3: 默认前缀改 ghfast.top + 一次性迁移存量旧默认

**Files:**
- Modify: `src-tauri/src/settings.rs:6`(默认常量)、新增 `LEGACY_MIRROR_PREFIX` 与 `migrate_mirror_prefix()`
- Modify: `src-tauri/src/lib.rs:2801-2813`(setup 调用迁移)
- Test: `src-tauri/src/settings.rs`(内联 `#[cfg(test)]`)

**Interfaces:**
- Produces:`pub fn migrate_mirror_prefix(app_data: &Path) -> anyhow::Result<Settings>` —— 若持久化 `mirror_prefix == LEGACY_MIRROR_PREFIX` 则抬到 `DEFAULT_MIRROR_PREFIX`,幂等;返回落盘后的 Settings。

- [ ] **Step 1: 写失败测试** —— `src-tauri/src/settings.rs` 的 `mod tests` 内新增:

```rust
    #[test]
    fn migrate_bumps_legacy_prefix_to_new_default() {
        let tmp = tempfile::tempdir().unwrap();
        // 存量:旧默认 ghproxy.net
        std::fs::write(
            tmp.path().join("settings.json"),
            format!(r#"{{"mirror_enabled":true,"mirror_prefix":"{LEGACY_MIRROR_PREFIX}"}}"#),
        )
        .unwrap();
        let got = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(got.mirror_prefix, DEFAULT_MIRROR_PREFIX, "旧默认应被抬到新默认");
        assert_eq!(load(tmp.path()).mirror_prefix, DEFAULT_MIRROR_PREFIX, "已持久化");
    }

    #[test]
    fn migrate_leaves_non_legacy_prefix_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        // 非旧默认值(模拟用户/未来自定义)不应被迁移改动。
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"mirror_enabled":true,"mirror_prefix":"https://custom.example/"}"#,
        )
        .unwrap();
        let got = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(got.mirror_prefix, "https://custom.example/", "自定义值不动");
    }

    #[test]
    fn migrate_is_idempotent_on_new_default() {
        let tmp = tempfile::tempdir().unwrap();
        // 无文件:load 得新默认;迁移后仍是新默认,不误改。
        let got = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(got.mirror_prefix, DEFAULT_MIRROR_PREFIX);
        let again = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(again.mirror_prefix, DEFAULT_MIRROR_PREFIX);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test -p voice-notes migrate_ 2>&1 | tail -20`
Expected: 编译失败 `cannot find value 'LEGACY_MIRROR_PREFIX'` / `cannot find function 'migrate_mirror_prefix'`。

- [ ] **Step 3: 改默认常量 + 加旧默认常量** —— `src-tauri/src/settings.rs:6`,替换该行为:

```rust
pub const DEFAULT_MIRROR_PREFIX: &str = "https://ghfast.top/";
/// 旧默认前缀(v0.4.1 及之前)。仅用于一次性迁移判定:UI 从不允许编辑前缀,故存量等于此
/// 值者必是旧默认而非用户自定义,可安全抬到新默认。
pub const LEGACY_MIRROR_PREFIX: &str = "https://ghproxy.net/";
```

- [ ] **Step 4: 加迁移函数** —— `src-tauri/src/settings.rs`,在 `update(...)`(205-211)之后加:

```rust
/// 一次性迁移:存量 mirror_prefix 若等于旧默认(ghproxy.net),抬到新默认(ghfast.top)。
/// 幂等——非旧默认值(新默认 / 未来自定义)不动。走 update 复用 WRITE_LOCK 串行化。
pub fn migrate_mirror_prefix(app_data: &Path) -> anyhow::Result<Settings> {
    update(app_data, |s| {
        if s.mirror_prefix == LEGACY_MIRROR_PREFIX {
            s.mirror_prefix = DEFAULT_MIRROR_PREFIX.into();
        }
    })
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test -p voice-notes migrate_ 2>&1 | tail -20`
Expected: 三个 `migrate_*` PASS。

- [ ] **Step 6: 确认既有默认相关测试仍绿** —— 现有 `load_missing_or_corrupt_falls_back_to_default`(断言默认 == `DEFAULT_MIRROR_PREFIX`)与默认值随常量变化仍成立:

Run: `cd src-tauri && cargo test -p voice-notes settings:: 2>&1 | tail -20`
Expected: 全 PASS。

- [ ] **Step 7: setup 中调用迁移** —— `src-tauri/src/lib.rs`,在 `.setup(|app| {` 内、`let s = app_data.as_ref().map(...)`(约 2813)**之前**插入(即紧接 2810-2812 的日志重定向块之后):

```rust
            // 一次性迁移:把存量旧默认镜像前缀抬到新默认(见 settings::migrate_mirror_prefix)。
            // 必须先于本函数后续的 settings::load,使其读到迁移后的值。
            if let Some(dir) = &app_data {
                let _ = settings::migrate_mirror_prefix(dir);
            }
```

- [ ] **Step 8: 编译确认**

Run: `cd src-tauri && cargo build -p voice-notes 2>&1 | tail -15`
Expected: 编译通过(无新错误)。

- [ ] **Step 9: 提交**

```bash
git add src-tauri/src/settings.rs src-tauri/src/lib.rs
git commit -m "feat(settings): 默认镜像前缀改 ghfast.top + 一次性迁移旧默认"
```

---

### Task 4: 并发下载 + 失败隔离

**Files:**
- Modify: `src-tauri/src/lib.rs:33`(新增常量)、新增 `download_one()`、重写 `download_models` 下载段(约 2173-2234)

**Interfaces:**
- Consumes:Task 2 的 `download_urls`;已有 `download_artifact(a, root, url, cancel, emit)`、`retryable_download_error`、`preload_models`、`ResetOnDrop`、`ipc::ModelDownloadEvent`。
- Produces:并发不改前端契约——仍按 `artifact` id 发 `model_download` 事件,全成功后发 `artifact:"all", phase:"done"`。

- [ ] **Step 1: 加并发上限常量** —— `src-tauri/src/lib.rs`,在 `DOWNLOAD_ATTEMPTS_PER_URL`(33)旁加:

```rust
/// 同时下载的模型工件数上限。大文件占带宽,取小值折中;不做用户可配。
const MAX_CONCURRENT_DOWNLOADS: usize = 3;
```

- [ ] **Step 2: 加 `download_one` 辅助函数** —— `src-tauri/src/lib.rs`,在 `download_models` 函数**之前**加(把单工件的「多代理回退 + 代理少重试/原站多重试」逻辑收拢成可复用函数):

```rust
/// 下载单个工件:按 download_urls 的候选顺序尝试。代理候选各试 1 次(死代理快速跳过,
/// 压回退延迟),原站(候选列表最后一项,== a.url)给足 DOWNLOAD_ATTEMPTS_PER_URL 次。
/// 返回 Err(msg):msg=="cancelled" 表示被取消,其余为可展示错误文案。
fn download_one(
    a: &models::Artifact,
    root: &std::path::Path,
    mirror_enabled: bool,
    mirror_prefix: &str,
    cancel: &std::sync::atomic::AtomicBool,
    emit: &impl Fn(&str, &str, u64, u64, &str),
) -> Result<(), String> {
    let urls = models::download::download_urls(a.url, mirror_enabled, mirror_prefix);
    let mut last_err: Option<String> = None;
    for url in &urls {
        // 原站(无前缀,恒等于 a.url)多重试;代理候选各 1 次快速跳过。
        let attempts = if url == a.url { DOWNLOAD_ATTEMPTS_PER_URL } else { 1 };
        for attempt in 1..=attempts {
            match models::download::download_artifact(a, root, url, cancel, emit) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if msg == "cancelled" {
                        return Err("cancelled".into());
                    }
                    let retryable = models::download::retryable_download_error(&msg);
                    last_err = Some(format!("{url}: {msg}"));
                    if !retryable || attempt == attempts {
                        break; // 换下一个候选 URL
                    }
                }
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("cancelled".into());
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "下载失败".into()))
}
```

- [ ] **Step 3: 重写 `download_models` 下载段** —— `src-tauri/src/lib.rs`,把从 `// preload 需要 app...`(约 2173)到 `preload_models(...)` 结束(约 2234)、即当前「单 `emit` 闭包 + `for a in selected` 串行循环 + 尾部 done」整段,替换为并发版:

```rust
        // preload 需要 app,但 app 随即被 worker 闭包 clone 走,先克隆留给补预载与 done 事件。
        let app_pl = app.clone();
        let app_done = app.clone();
        let mirror_enabled = s.mirror_enabled;
        let mirror_prefix = s.mirror_prefix.clone();
        let items: Vec<&models::Artifact> = selected; // ARTIFACTS 原顺序,进度/展示稳定
        let next = std::sync::atomic::AtomicUsize::new(0);
        let all_ok = std::sync::atomic::AtomicBool::new(true);
        let worker_count = items.len().min(MAX_CONCURRENT_DOWNLOADS).max(1);
        // scope:worker 借用 items/next/all_ok/cancel/root,块结束自动 join,无需 Arc。
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let app_w = app.clone();
                let cancel = &cancel;
                let next = &next;
                let all_ok = &all_ok;
                let root = &root;
                let items = &items;
                let mirror_prefix = mirror_prefix.as_str();
                scope.spawn(move || {
                    let emit = |id: &str, phase: &str, received: u64, total: u64, message: &str| {
                        let _ = app_w.emit(
                            "model_download",
                            ipc::ModelDownloadEvent {
                                artifact: id.into(),
                                phase: phase.into(),
                                received_bytes: received,
                                total_bytes: total,
                                message: message.into(),
                            },
                        );
                    };
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= items.len() {
                            break;
                        }
                        let a = items[i];
                        if models::artifact_present(root, a) {
                            continue;
                        }
                        match download_one(a, root, mirror_enabled, mirror_prefix, cancel, &emit) {
                            Ok(()) => {}
                            Err(msg) if msg == "cancelled" => {
                                emit(a.id, "cancelled", 0, 0, "cancelled");
                                all_ok.store(false, Ordering::SeqCst);
                                break; // 取消:本 worker 停止取新工件
                            }
                            Err(msg) => {
                                // 失败隔离:标记整体失败,但继续下载其余工件(不再连带中断)。
                                emit(a.id, "error", 0, 0, &msg);
                                all_ok.store(false, Ordering::SeqCst);
                            }
                        }
                    }
                });
            }
        });
        drop(guard); // 复位先于 done 事件,保持"收到 done 即可再次下载"的时序
        if all_ok.load(Ordering::SeqCst) {
            let _ = app_done.emit(
                "model_download",
                ipc::ModelDownloadEvent {
                    artifact: "all".into(),
                    phase: "done".into(),
                    received_bytes: 0,
                    total_bytes: 0,
                    message: String::new(),
                },
            );
            // 补齐后立即预载,无需重启即可开录。
            preload_models(app_pl, session, recognizer_cache, embedder_cache);
        }
```

- [ ] **Step 4: 编译确认**

Run: `cd src-tauri && cargo build -p voice-notes 2>&1 | tail -25`
Expected: 编译通过。若报 `cancel`/`Ordering` 相关借用或类型错误,核对:`cancel` 为 `Arc<AtomicBool>`,worker 内 `&cancel` 经 Deref 强制转 `&AtomicBool` 传入 `download_one` 与 `.load`;`Ordering` 已在文件顶部 `use`。

- [ ] **Step 5: 跑既有下载相关单测(确保未回归)**

Run: `cd src-tauri && cargo test -p voice-notes 2>&1 | tail -20`
Expected: 全绿(含 Task 1-3 新增用例)。

- [ ] **Step 6: 人工冒烟(网络路径,按仓库惯例不做单测)**

在开发机运行 `npm run tauri dev`,到设置页「语音模型」:
- 删除若干已下载模型 → 触发多个模型下载,确认**多个进度条同时推进**(并发生效),且各自独立到达「已下载」。
- 断网或临时把 `BACKUP_MIRROR_PREFIXES`/主前缀之一改成不可达域名,确认**单个模型失败后其余仍继续下载完成**(失败隔离),失败项显示 error。
- 下载中点「取消」,确认在途工件停止、状态复位、可再次发起下载。
Expected: 上述行为全部符合。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(download): 模型下载改有界并发 + 失败隔离,抽出 download_one"
```

---

### Task 5: 前端模型行展开显示下载地址

**Files:**
- Modify: `src/routes/settings/+page.svelte`(脚本区加状态与 `effectiveUrl`;模型行 760-802 加展开)

**Interfaces:**
- Consumes:Task 1 的 `a.url`(`ArtifactState.url`);已有 `settings.mirror_enabled` / `settings.mirror_prefix`。

- [ ] **Step 1: 加展开状态与地址拼接函数** —— `src/routes/settings/+page.svelte` 脚本区(与其它 `$state` 声明相邻处,如 `mirrorTest` 附近 38 行左右)加:

```svelte
  let expandedId = $state<string | null>(null);

  /** 镜像开启时返回「前缀+原始url」(等同后端 apply_mirror);关闭/空前缀返回原始 url。 */
  function effectiveUrl(url: string): string {
    const p = (settings?.mirror_prefix ?? "").trim();
    if (!settings?.mirror_enabled || !p) return url;
    return p.endsWith("/") ? `${p}${url}` : `${p}/${url}`;
  }
```

- [ ] **Step 2: 模型行加展开触发 + 详情面板** —— 同文件,把模型行 `{#each status.artifacts as a (a.id)}` 内的 `<div class="row">…</div>`(760-802)整体替换为(在 `.row-info` 内标签加可点击展开箭头,并在 `.row` 之后条件渲染详情面板):

```svelte
        {#each status.artifacts as a (a.id)}
          <div class="row">
            <div class="row-info">
              <button
                class="url-toggle"
                aria-expanded={expandedId === a.id}
                aria-label={expandedId === a.id ? "收起下载地址" : "展开下载地址"}
                onclick={() => (expandedId = expandedId === a.id ? null : a.id)}
              >
                <span class="caret" class:open={expandedId === a.id}>▸</span>
                <span class="row-label">{a.label} · 约 {a.approx_mb}MB</span>
              </button>
              {#if a.present}
                <span class="present">已下载</span>
              {/if}
            </div>

            {#if prog[a.id]}
              <div class="dl">
                <span class="phase">
                  {phaseText[prog[a.id].phase] ?? prog[a.id].phase}
                  {#if prog[a.id].phase === "downloading" && prog[a.id].total > 0}
                    {mb(prog[a.id].received)}/{mb(prog[a.id].total)}MB
                  {/if}
                </span>
                <div class="bar"><div class="fill" style="width:{pct(prog[a.id])}%"></div></div>
              </div>
            {:else if a.present}
              {#if confirmDeleteId === a.id}
                <div class="confirm-inline">
                  <button class="link danger" onclick={() => doDelete(a.id)}>确认删除</button>
                  <button class="link" onclick={() => (confirmDeleteId = null)}>取消</button>
                </div>
              {:else}
                <button
                  class="link danger row-action"
                  disabled={recording.isLive || downloadingActive}
                  title={recording.isLive
                    ? "录制中不能删除模型"
                    : downloadingActive
                      ? "下载进行中不能删除模型"
                      : "删除本模型(可随时重新下载)"}
                  onclick={() => {
                    confirmDeleteId = a.id;
                  }}>删除</button
                >
              {/if}
            {:else}
              <button class="btn-secondary" onclick={() => download(a.id)}>下载</button>
            {/if}
          </div>
          {#if expandedId === a.id}
            <div class="url-detail">
              <div class="url-line">
                <span class="url-tag">原始地址</span>
                <code class="url-text">{a.url}</code>
                <button class="link" onclick={() => navigator.clipboard.writeText(a.url)}>复制</button>
              </div>
              <div class="url-line">
                <span class="url-tag">镜像地址</span>
                {#if settings?.mirror_enabled && (settings?.mirror_prefix ?? "").trim()}
                  <code class="url-text">{effectiveUrl(a.url)}</code>
                  <button class="link" onclick={() => navigator.clipboard.writeText(effectiveUrl(a.url))}>复制</button>
                {:else}
                  <span class="url-muted">未启用镜像加速</span>
                {/if}
              </div>
            </div>
          {/if}
        {/each}
```

- [ ] **Step 3: 加样式** —— 同文件 `<style>` 区末尾加(跟随现有 `.row` / `.link` 体系,长 URL 横向滚动不撑破布局):

```css
  .url-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    color: inherit;
    font: inherit;
    text-align: left;
  }
  .caret {
    display: inline-block;
    transition: transform 0.15s ease;
    opacity: 0.6;
    font-size: 0.85em;
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .url-detail {
    padding: 6px 0 10px 20px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    border-bottom: 1px solid var(--hairline, rgba(128, 128, 128, 0.18));
  }
  .url-line {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .url-tag {
    flex: 0 0 auto;
    font-size: 0.8em;
    opacity: 0.6;
  }
  .url-text {
    flex: 1 1 auto;
    min-width: 0;
    overflow-x: auto;
    white-space: nowrap;
    font-size: 0.8em;
    opacity: 0.85;
  }
  .url-muted {
    font-size: 0.8em;
    opacity: 0.5;
  }
```

> 注:`--hairline` 若项目未定义则回退到内联 rgba(已写在 `var(...)` 第二参)。若样式区已有等价的细线变量(如 `.row` 用的分隔色),优先复用该变量名替换之。

- [ ] **Step 4: 类型检查**

Run: `npm run check 2>&1 | tail -20`(若无 `check` 脚本则 `npx svelte-check --threshold error 2>&1 | tail -20`)
Expected: 无类型错误(`a.url` 已在 Task 1 加入类型)。

- [ ] **Step 5: 人工冒烟**

`npm run tauri dev` → 设置页「语音模型」:
- 点任一模型行标签 → 展开显示「原始地址」(GitHub 直链)+「镜像地址」;箭头旋转;再点收起;点另一行时前一行自动收起(手风琴)。
- 「复制」按钮可复制对应地址。
- 关闭「镜像加速」开关 → 展开行的镜像地址显示「未启用镜像加速」;开启后显示 `ghfast.top` 前缀拼接地址。
- DTLN 两行原始地址显示 `SoulZhong/voice-notes` 链接。
Expected: 全部符合;长 URL 在其行内横向滚动,不撑破页面。

- [ ] **Step 6: 提交**

```bash
git add src/routes/settings/+page.svelte
git commit -m "feat(settings-ui): 模型行可展开显示原始/镜像下载地址"
```

---

## Self-Review 结论

- **Spec 覆盖**:模块一(显示)= Task 1+5;模块二(多代理加速 + 默认切换 + 迁移)= Task 2+3;模块三(并发)= Task 4;带外 DTLN 修复已完成,维护注释 = Task 1 Step 6。无遗漏。
- **占位扫描**:无 TBD;所有代码步骤含完整代码;死代理快速跳过策略已在 Task 4 `download_one` 用「代理 1 次 / 原站多次」具体化。
- **类型一致**:`ArtifactState.url`(Rust `String` / TS `string`)贯穿 Task 1→5;`download_urls` 签名不变、语义扩展,Task 4 依赖「原站恒最后」;`migrate_mirror_prefix` 返回 `Settings` 与调用点一致;`BACKUP_MIRROR_PREFIXES` 常量名在 Task 2 定义、测试与实现一致。
