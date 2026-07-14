# 神经残余级（DTLN-aec）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 离线清洗在 AEC3 线性消除后追加 DTLN-aec 神经残余级，把 mic 轨残余互相关从 ~0.13/0.29 压到 ≤0.15（spike 实测 0.116），本人声音无损（spike -0.01dB）。

**Architecture:** 新纯模块 `audio/neural_aec.rs`（tract-tflite 双阶段推理 + 官方块算法移植）+ 模型注册表两条目 + `echo_clean` 的 `Ok(Some)` 路径追加一级 + `CleanInfo.neural` 观测。任何失败保留 AEC3 输出照常落盘。

**Tech Stack:** Rust;新依赖 `tract-tflite`(纯 Rust,T1 可行性硬闸,失败切 `tflitec`)与 `realfft`(纯 Rust FFT);DTLN-aec 官方 256 档 TF-Lite 模型(MIT 含权重)。

**规格:** `docs/superpowers/specs/2026-07-14-voice-notes-neural-residual-aec-design.md`
**分支:** `soft-aec-p3b`(叠栈于 soft-aec-p2;#41/#43 合入后改基)

## Global Constraints

- 增值层三连:模型不在场→跳过;加载/推理失败→保留 AEC3 输出;永不 panic、不阻塞转码。
- 样本守恒:神经级输出 len == 输入 len。
- 触发条件:仅 AEC3 清洗实际执行(`Ok(Some)`)的路径追加;未检出回声的笔记不动。
- 模型工件哈希钉死(下载后校验,与 whisper 工件同哲学)——**ONNX 路线定版**:
  - `dtln_aec_256_1.onnx` (5.5MB) sha256 `61250b397616146e79371b58b34da068ce0adb09f43edfac5421f4faf6990917`
  - `dtln_aec_256_2.onnx` (10MB) sha256 `b79a9efca5b7e33e6bbd088acc60fc946250b23e104b103c47a24783a0c0b13a`
  - URL 前缀 `https://github.com/SoulZhong/voice-notes/releases/download/models-dtln-aec-v1/`
  - 转换工序(可复现):tf2onnx 1.17.0 + tf 2.21.0,`python -m tf2onnx.convert --tflite <m>.tflite --output <m>.onnx --opset 13`;与原 tflite 随机输入数值最大差 ~1e-6
- 依赖:`tract-onnx = "0.21"`(纯 Rust) + `realfft = "3"`;**不引入** tract-tflite/tflitec(硬闸双败,见偏差记录)
- 模型依赖测试一律 `#[ignore]`(与既有 7 个模型依赖 ignored 同惯例),开发机通过 `VN_DTLN_DIR` 指向本地模型目录运行(spike 已有一份:scratchpad/DTLN-aec/pretrained_models)。
- 提交信息中文、动机导向,不加任何 Co-Authored-By / Generated-with 尾注。
- 每任务结束 `cd src-tauri && cargo test` 全绿再提交(分支基线 447 lib + 1 集成,8 ignored)。

## 官方块算法（T2 移植的唯一真值,提炼自 run_aec.py）

```
block_len=512, block_shift=128, 16kHz
两端各 pad (block_len-block_shift)=384 个零;len_audio 记原长
states_1/states_2 = 全零(形状取模型输入张量声明)
in_buffer/in_buffer_lpb/out_buffer = 零缓冲(512)
num_blocks = (padded_len - 384) / 128
逐块:
  in_buffer 左移 128,尾部填 mic 新 128 样本;lpb 同法
  fft_mic = rfft(in_buffer)  → mag_mic(257)
  fft_lpb = rfft(in_buffer_lpb) → mag_lpb(257)
  模型1(mag_mic, states_1, mag_lpb) → (mask(257), states_1')
  est = irfft(fft_mic * mask)   ← 注意 numpy irfft 含 1/N 归一;realfft C2R 不含,需 ÷512
  模型2(est(512), states_2, in_buffer_lpb(512)) → (out_block(512), states_2')
  out_buffer 左移 128 尾部清零,+= out_block
  输出收集 out_buffer[..128]
最后去 pad:输出取 [..len_audio](左 pad 384 已由收集时序抵消,详见 run_aec.py 尾部)
```

模型张量顺序按 python interpreter 的 input_details 序:模型1 输入 [0]=mag_mic [1]=states [2]=mag_lpb,输出 [0]=mask [1]=states';模型2 输入 [0]=est [1]=states [2]=lpb_time,输出 [0]=out [1]=states'。**tract 的输入序可能与之不同——T1 必须打印双模型全部输入/输出的名字+形状,T2 按名字/形状映射,不许赌下标。**

## 文件结构

| 文件 | 职责 |
|---|---|
| Create `src-tauri/src/audio/neural_aec.rs` | tract-tflite 加载/双阶段块推理/suppress_residual 纯入口 |
| Modify `src-tauri/Cargo.toml` | 增 tract-tflite、realfft |
| Modify `src-tauri/src/models/mod.rs` | 注册表增 dtln_aec_256 两工件 |
| Modify `src-tauri/src/audio/echo_clean.rs` | Ok(Some) 路径追加神经级+前后 NCC 日志 |
| Modify `src-tauri/src/store/audio.rs` | CleanInfo 增 `neural: Option<bool>` |
| Modify `src-tauri/src/store/transcode.rs` | 透传 neural 到 CleanInfo |

---

### Task 1: 可行性硬闸——tract-onnx 加载并跑通一块(ONNX 路线定版)

**Files:**
- Modify: `src-tauri/Cargo.toml`(`tract-onnx = "0.21"` + `realfft = "3"`;清掉工作树残留的 tract-tflite)
- Create: `src-tauri/src/audio/neural_aec.rs`(仅骨架:加载+单块前向)
- Modify: `src-tauri/src/audio/mod.rs`(挂 `pub mod neural_aec;`)

**Interfaces:**
- Produces: `pub struct DtlnAec { /* 两个 tract Plan + 状态形状 */ }`;`pub fn load(models_dir: &Path) -> anyhow::Result<DtlnAec>`(找 `dtln_aec_256_1.onnx`/`_2.onnx`,缺文件报错带路径)

- [ ] **Step 1: 写 #[ignore] 硬闸测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 可行性硬闸:tract-tflite 能加载官方双模型并对全零输入出数。
    /// 运行: VN_DTLN_DIR=<模型目录> cargo test neural_aec -- --ignored --nocapture
    #[test]
    #[ignore]
    fn models_load_and_run_one_block() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").expect("需设 VN_DTLN_DIR"));
        let aec = load(&dir).expect("双模型应可加载");
        let out = aec.run_block_for_test(&[0.0f32; 512], &[0.0f32; 512]).expect("全零单块应出数");
        assert_eq!(out.len(), 512, "块输出长度");
        assert!(out.iter().all(|v| v.is_finite()), "输出应全有限");
    }

    /// 无模型目录:load 报错带路径,不 panic。
    #[test]
    fn load_missing_models_errs_gracefully() {
        let e = load(std::path::Path::new("/nonexistent-vn-dtln")).unwrap_err();
        assert!(format!("{e:#}").contains("dtln_aec_256"), "错误应点名缺失文件");
    }
}
```

`run_block_for_test`:pub(crate) 测试探针,内部即 T2 的单块流程(T1 先做最小可用:双模型前向+状态回喂一次)。

- [ ] **Step 2: 实现骨架并跑硬闸**

Run: `cd src-tauri && VN_DTLN_DIR=/private/tmp/claude-501/-Users-teemo-workspace-soul-voice-notes/9d8a6b78-85d9-44bc-856b-738c3afa5d39/scratchpad cargo test neural_aec -- --ignored --nocapture`

实现要点:
- `tract_onnx::onnx().model_for_path(..)`(→ into_optimized → into_runnable;TDim 转 usize 用 to_i64,spike 踩过 to_usize 不存在),打印双模型全部输入/输出 name/shape(eprintln,T2 依赖);
- 状态张量形状从模型声明读取,全零初始化;
- 单块:按「官方块算法」一节走一遍(FFT 用 realfft;此步一并加 `realfft = "3"` 依赖)。

**硬闸判定:** ONNX 路线已由控制者 spike 验证(scratchpad/tract-check 工程,双模型 into_optimized+run 全零块出数);若正式接入仍遇算子问题,报 BLOCKED 附确切报错,不得自行换路线。已知形状:模型1 in[mag(1,1,257),states(1,2,256,2),lpb_mag(1,1,257)] out[mask(257),states'];模型2 in[est(1,1,512),states(1,2,256,2),lpb(1,1,512)] out[out(512),states']。张量绑定按名字映射(tf2onnx 保留 tflite 输入名:模型1 input_3/input_5/input_4 分别为 mag/states/lpb_mag——名字与顺序不一致,勿赌下标)。

- [ ] **Step 3: 全量回归(未设 VN_DTLN_DIR 时新测试仅 1 个非 ignore 且应绿)**

Run: `cd src-tauri && cargo test`
Expected: 全绿;ignored 计数 +1

- [ ] **Step 4: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/audio/neural_aec.rs src-tauri/src/audio/mod.rs
git commit -m "神经残余级接入tract-onnx:双模型单块出数,tflite两路线硬闸双败改ONNX(详见偏差记录)"
```

---

### Task 2: 推理引擎——完整块处理与 suppress_residual

**Files:**
- Modify: `src-tauri/src/audio/neural_aec.rs`

**Interfaces:**
- Produces: `pub fn suppress_residual(mic: &[f32], reference: &[f32], models_dir: &Path) -> anyhow::Result<Vec<f32>>` — 输出 len == mic.len();reference 短则零垫齐、长则截断(与官方 lpb 对齐语义一致)

- [ ] **Step 1: 写 #[ignore] 行为测试**

```rust
    /// 回声衰减:mic=reference 的 200ms 延迟拷贝(纯回声),输出能量应显著低于输入。
    #[test]
    #[ignore]
    fn suppresses_delayed_echo() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").unwrap());
        let mut seed = 9u64;
        let reference = crate::audio::delay_estimate::tests::block_modulated_noise(16_000 * 10, &mut seed);
        let delay = 3200;
        let mut mic = vec![0.0f32; reference.len()];
        for i in delay..mic.len() {
            mic[i] = reference[i - delay] * 0.3;
        }
        let out = suppress_residual(&mic, &reference, &dir).unwrap();
        assert_eq!(out.len(), mic.len(), "样本守恒");
        let p = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32;
        let tail = mic.len() / 2; // 后半(收敛后)
        assert!(p(&out[tail..]) < p(&mic[tail..]) / 4.0, "回声应至少压 6dB");
    }

    /// 参考全零:近端直通,能量保持在 ±3dB(神经模型对无回声输入不该乱削)。
    #[test]
    #[ignore]
    fn near_end_passes_when_reference_silent() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").unwrap());
        let mut seed = 21u64;
        let mic = crate::audio::delay_estimate::tests::block_modulated_noise(16_000 * 10, &mut seed);
        let zeros = vec![0.0f32; mic.len()];
        let out = suppress_residual(&mic, &zeros, &dir).unwrap();
        assert_eq!(out.len(), mic.len());
        let p = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32;
        let tail = mic.len() / 2;
        let ratio = p(&out[tail..]) / p(&mic[tail..]);
        assert!(ratio > 0.5 && ratio < 2.0, "近端应保持 ±3dB: {ratio}");
    }

    /// 零散长度守恒:非 128 倍数输入,输出长度仍严格相等。
    #[test]
    #[ignore]
    fn conserves_arbitrary_length() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").unwrap());
        let mic = vec![0.01f32; 16_000 + 77];
        let reference = vec![0.0f32; 16_000 + 77];
        let out = suppress_residual(&mic, &reference, &dir).unwrap();
        assert_eq!(out.len(), mic.len());
    }
```

- [ ] **Step 2: 实现完整引擎**

按「官方块算法」一节逐行移植(该节即为本步代码的规格,含 pad/移位/FFT/掩码/两级推理/overlap-add/去 pad 全部细节与 irfft ÷512 归一陷阱);状态张量逐块回喂;张量绑定按 T1 打印的名字/形状映射。`suppress_residual` 内部 `load` 一次、复用 plan(45 分钟音频约 34 万块,plan 必须只建一次)。

Run: `cd src-tauri && VN_DTLN_DIR=<同T1> cargo test neural_aec -- --ignored --nocapture`
Expected: 4 个 ignored 测试全 PASS(含 T1 的)

- [ ] **Step 3: 全量回归 + 提交**

```bash
git add src-tauri/src/audio/neural_aec.rs
git commit -m "神经残余级推理引擎:官方块算法移植,守恒/回声衰减/近端直通实测过闸"
```

---

### Task 3: 模型注册表接入

**Files:**
- Modify: `src-tauri/src/models/mod.rs`

**Interfaces:**
- Produces: 注册表新 Artifact(`dtln_aec_256`),镜像/下载/校验走既有机制;`models::root()` 下落地两 tflite 文件

- [ ] **Step 1: 照抄 silero 条目形状增加(不压缩包,两个裸 onnx 文件,URL/哈希用 Global Constraints 定版值,以现有结构最贴近者为准)**

URL 与 sha256 用 Global Constraints 钉死值。UI 侧模型页若为注册表驱动(检查前端 models 页数据源),新增条目应自动出现;若有白名单需同步——按实际情况处理并入报告。

- [ ] **Step 2: 真机验证下载与校验**

Run: 通过既有下载命令/入口拉取(或手动触发 download.rs 对应函数的测试路径),校验 sha256 一致、落位 `models::root()`。

- [ ] **Step 3: 全量回归 + 提交**

```bash
git add src-tauri/src/models/mod.rs
git commit -m "模型注册表增DTLN-aec 256档双工件:哈希钉死走既有镜像下载校验"
```

---

### Task 4: 清洗管线接入与观测

**Files:**
- Modify: `src-tauri/src/audio/echo_clean.rs`
- Modify: `src-tauri/src/store/audio.rs`(CleanInfo 增字段)
- Modify: `src-tauri/src/store/transcode.rs`(透传)

**Interfaces:**
- Produces: `CleanReport` 增 `pub neural: bool`;`CleanInfo` 增 `pub neural: Option<bool>`(serde default + skip_serializing_if)

- [ ] **Step 1: 写测试**

```rust
    /// 模型不在场:clean_wav 照常产出(AEC3-only),report.neural == false。
    /// (复用既有 cleans_600ms 测试的合成与断言,仅加 neural 断言——
    ///  测试环境 models::root() 无 dtln 文件,天然走跳过路径。)
```

在既有 `cleans_600ms_bluetooth_echo_and_keeps_local_voice` 断言尾部追加 `assert!(!report.neural, "无模型时神经级不应标记");`;store::audio 的 roundtrip 测试给 CleanInfo 加 `neural: Some(true)` 往返断言;旧 json 兼容测试确认缺字段可读。

- [ ] **Step 2: 实现**

`clean_wav` 的写盘之前(`Ok(Some)` 路径,`cleaned` 就绪后):

```rust
    // 神经残余级(增值层):模型在场才跑;失败保留 AEC3 输出。
    let mut neural = false;
    let dtln_dir = crate::models::root();
    if dtln_dir.join("dtln_aec_256_1.onnx").exists() {
        match crate::audio::neural_aec::suppress_residual(&cleaned, &system_aligned, &dtln_dir) {
            Ok(out) => {
                let before = residual_peak(&system_aligned, &cleaned);
                let after = residual_peak(&system_aligned, &out);
                eprintln!("神经残余级完成: 残余互相关 {before:.3} -> {after:.3}");
                cleaned = out;
                neural = true;
            }
            Err(e) => eprintln!("神经残余级失败,保留 AEC3 输出: {e:#}"),
        }
    }
```

`residual_peak` 小助手:包络 + `delay_estimate::estimate_delay(...,1200)` 取 peak,None 时 0.0。`CleanReport`/`CleanInfo`/transcode 透传按 Interfaces。注意 `cleaned` 需改 `let mut`。

- [ ] **Step 3: 全量回归(无模型环境全部非 ignore 测试必须绿) + 提交**

```bash
git add src-tauri/src/audio/echo_clean.rs src-tauri/src/store/audio.rs src-tauri/src/store/transcode.rs
git commit -m "清洗管线接入神经残余级:模型在场即启用,前后残余互相关入日志,neural入meta"
```

---

### Task 5: 真实录音验收(spike 同口径)

**Files:** 无生产代码改动;`echo_clean.rs` 可加 `#[ignore]` 验收测试

- [ ] **Step 1: 模型落位** 把 spike 的两个 tflite 拷入 `models::root()`(或走 T3 下载)。

- [ ] **Step 2: 同切片复验** 用 `VN_NOTE_DIR`/直接调用对 spike 同一真实笔记切片跑 Rust 全管线,断言口径:
- 残余互相关 ≤0.15(spike 0.116;Rust 与 python 差 <0.02)
- 本人独讲能量保持 |Δ|≤0.5dB
- 吞吐 ≥10 倍实时

- [ ] **Step 3: 全量回归 + 真机端到端** 沙箱实例录一段带回声会话(内置外放放音),停录观察日志顺序:`离线回声清洗完成` → `神经残余级完成: 残余互相关 X -> Y`;audio.json 的 clean.neural==true。

- [ ] **Step 4: 收尾提交(偏差记录/注释修正,如有)**
