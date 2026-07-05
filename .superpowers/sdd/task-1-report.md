# Task 1 报告: settings 扩展三字段

## 实现内容

`src-tauri/src/settings.rs`:
- `Settings` 新增三字段:
  - `pub data_dir: Option<String>` — `#[serde(default, skip_serializing_if = "Option::is_none")]`
  - `pub models_dir: Option<String>` — 同上
  - `pub asr_model: String` — `#[serde(default = "default_asr")]`,默认 `ASR_SENSE_VOICE`
- 新增常量:`pub const ASR_SENSE_VOICE: &str = "sense_voice";`、`pub const ASR_WHISPER: &str = "whisper";`
- 新增纯函数 `pub fn resolve_data_root(app_data: &Path, s: &Settings) -> PathBuf`:`data_dir` 非空(`Some` 且非空字符串)时取之,否则回退 `app_data`。
- `Default for Settings` 同步补齐三个新字段。
- `use std::path::Path` 改为 `use std::path::{Path, PathBuf}`。
- 因新增了 `asr_model` 等无默认 trait 派生的必填字段,原有测试 `save_then_load_roundtrip` 中的结构体字面量补了 `..Default::default()` 以保持编译(未改变其断言逻辑)。
- `ASR_WHISPER` 常量与 `resolve_data_root` 函数在本任务内尚无调用方(留给后续「数据目录解析」「ASR 选型」任务接入 lib.rs),标注 `#[allow(dead_code)]` 并加中文注释说明,避免产生新警告。

## TDD 证据

**RED**(brief 提供的两个测试原样加入后):
```
$ cargo test settings
error[E0609]: no field `data_dir` on type `settings::Settings`
error[E0609]: no field `models_dir` on type `settings::Settings`
error[E0609]: no field `asr_model` on type `settings::Settings`
error[E0425]: cannot find function `resolve_data_root` in this scope
error[E0560]: struct `settings::Settings` has no field named `data_dir`
error: could not compile `voice-notes` (lib test) due to 16 previous errors
```
(编译失败,符合预期 — 字段/常量/函数尚不存在)

**GREEN**(实现后):
```
$ cargo test settings
running 4 tests
test settings::tests::resolve_data_root_prefers_configured ... ok
test settings::tests::load_missing_or_corrupt_falls_back_to_default ... ok
test settings::tests::save_then_load_roundtrip ... ok
test settings::tests::new_fields_default_and_roundtrip ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 182 filtered out
```

**无新警告核对**(对比改动前后,均只跑一次 `cargo test settings`):
- 改动前(git stash 后跑):`warning: struct MockCapture is never constructed` + `warning: associated function from_wav is never used`(2 条,均为 pre-existing,与本任务无关)。
- 改动后:同样只有这 2 条 warning,无新增。(第一版实现时曾短暂出现 `ASR_WHISPER never used` / `resolve_data_root never used` 两条新警告,已用 `#[allow(dead_code)]` + 说明性注释消除)

**全量测试**(提交前跑一次):
```
$ cargo test
... 184 passed; 0 failed; 2 ignored (lib unit tests)
... 其余 5 个 integration test 文件均为 1 ignored(需要模型/硬件,原本就是 ignored,未受影响)
```

## 文件改动
- `src-tauri/src/settings.rs`(唯一改动文件,69 行新增/3 行删除)

## Commit
- `d96b4a1` `feat(settings): data_dir/models_dir/asr_model 三字段与 data_root 解析`

## 自查(Self-review)

- **完整性**:brief 中 Interfaces 列出的 4 项(两个 Option 字段 + serde 属性、asr_model + default_asr、两个常量、resolve_data_root)均已实现,签名与 brief 完全一致。
- **风格一致性**:沿用文件既有的中文注释风格、`Default` 手写 impl 风格、`Path`/`PathBuf` 引入方式;未引入新的依赖或不必要的抽象。
- **纪律(YAGNI)**:未额外添加校验逻辑、未提前实现 lib.rs 接入(留给后续任务),`resolve_data_root` 保持纯函数、无副作用,仅按 brief 要求实现「非空取之否则回退」的最小逻辑。对「非空」的解释为 `Some` 且字符串非空(而非仅判断 `is_some()`),这是对 brief 措辞的合理引申,未在 brief 测试中被要求覆盖但不违反其断言。
- **测试**:RED→GREEN 证据完整,测试输出干净(无 stderr 异常),协议要求的「无新警告」已核实达成。

## 顾虑(Concerns)

1. 为了消除 `ASR_WHISPER`/`resolve_data_root` 的 dead_code 警告,添加了 `#[allow(dead_code)]`。这是本任务范围内(尚无消费方)与全局「无新警告」约束之间的正常张力,待后续任务(数据目录解析 / ASR 选型)在 lib.rs 接入后,应移除这两处 `#[allow(dead_code)]`。已在代码注释中标注该临时性,提醒后续任务处理。
2. `.superpowers/sdd/progress.md` 在工作区中已有未暂存的修改(与本任务无关,推测是编排流程预先写入的进度文件更新),遵照「只改 settings.rs / 不要动其他文件」的指示,本次提交未包含该文件,仍保留其未暂存状态,留给上游流程处理。
