# P1 行走骨架 — 进度账本

Plan: docs/superpowers/plans/2026-06-30-voice-notes-p1-walking-skeleton.md
Branch: p1-walking-skeleton

- Task 1: complete (commits 54550b7..168fe8b, review clean after fix) — 脚手架
- Task 2: complete (head 9a824b0, review clean) — AudioFrame/AudioCapture/to_mono
- Task 3: complete (head d6e9270, review clean) — 重采样
- Task 4: complete (head 61d9ff8, review clean; Important+2Minor fixed w/ burst test) — AccumulatingBuffer
- Task 5: complete (head 6de0717, review clean after fix) — Recognizer + Whisper
- Task 6: complete (head aa7db0f, review clean; Important silent-fail fixed via ready handshake) — 麦克风采集
- Task 7: complete (head 2ea25ac, review clean; race+drain-stop fixed) — 会话编排 + IPC + Mock
- Task 8: complete (head 6f50907, build clean) — 录制 UI

## Minor findings (for final review)
Base (branch start): c50efb7e9a8a4d3001d63d286be1e83095a7429f

### Notes
- Task 1: frontend template is SvelteKit (src/routes/+page.svelte, src/app.html), NOT App.svelte/main.ts — Task 8 must adapt UI to SvelteKit routes.
- Task 1 Minor (deferred): src-tauri/models/ not created; Task 5 fetch_models.sh creates it. (gitignored anyway)
- crate [lib] name = app_lib; sherpa-rs 0.6.8; time pinned 0.3.47 (Tauri/cookie workaround).
- Minor (Task 3, defer to final): resample.rs s0 unwrap_or(0.0) is dead branch — add `// idx < len by construction` comment; no upsample-path test (boundary guard untested).
- Task 5 fix: language must be "" (empty=auto-detect) in sherpa-onnx; plan brief's "auto" is INVALID ("Invalid language: auto"). Integration test passes with "".
- Minor (defer to final): test fixture is English-only ("Hello"); once multilingual confirmed, add a Chinese-containing fixture+assertion to exercise 中英混合 path. recognizer_it.rs comment dropped VN_MODELS hint (harmless, #[ignore] gates it).
- API note: sherpa-rs 0.6.8 WhisperRecognizer::transcribe(u32, &[f32]) -> WhisperRecognizerResult (not Result); ctor uses eyre. model tokens file is base-tokens.txt.
- Task 6 intake for Task 7: Microphone::start now BLOCKS until stream confirmed open, returns Err on failure. cpal::Stream is !Send (owned on bg thread). In lib.rs start_recording, run_pipeline returning Err must emit a "status" error event (don't swallow). Keep `pub mod asr;` so tests/recognizer_it.rs (app_lib::asr) still compiles.
- Minor (Task 8, defer/triage): stop() has no try/catch (backend stop is no-op; low risk).
- ALL 8 TASKS COMPLETE. Ready for final whole-branch review.

## Final whole-branch review (opus): READY to merge
- No must-fix. 4 known minors all OK-to-defer.
- Lifecycle verified sound; "start blocked until restart" is intentional (stop is P1 no-op).
- P2 backlog (forward concerns):
  1. Fast stream re-transcribes ENTIRE cumulative buffer every ~1.5s → O(n²) CPU + unbounded RAM (~64KB/s). Needs sliding fast-window + committed slow-segment model.
  2. bounded(256) sink + blocking send in cpal callback → audio glitches when ASR lags; use try_send/drop-oldest or move ASR off capture path.
  3. Wire real stop/cancel (hook already exists: Microphone stop_tx + AudioCapture::stop; thread a stop handle lib.rs→run_pipeline).
  4. Emit "recording" status only AFTER recognizer init (avoid recording→error flash).
  5. Add Chinese fixture + assertion to exercise 中英混合 path; end-to-end test pushing stereo/non-16k frames through run_pipeline.
  6. Trim likely-unused deps: thiserror, serde_json (verify first).
- LIVE SMOKE still required (human): speak mixed 中文+English, confirm Chinese chars appear.

## P1.5 — VAD 语句分段重构 (on branch p1-walking-skeleton, before merge)
Plan: docs/superpowers/plans/2026-07-01-voice-notes-p1.5-vad-segmentation.md
- T1: complete (head 5dcc0c4) — fetch silero_vad.onnx
- T2: complete (head b8e891d, review clean) — Segmenter trait + MockSegmenter
- T3: complete (head 878445f, review clean; 2 accepted minors) — SileroSegmenter
- T4: complete (head d7c6dfa, review clean; +partial assertion) — rewrite run_pipeline; delete buffer; ipc FinalEvent; lib.rs wired
- T5: complete (head 615bf65, review clean) — frontend final list + partial line (lib.rs wiring was folded into T4)

## P1.5 final review (opus): READY WITH MINOR FIXES → minors applied
- Confirmed: O(n²)+unbounded-memory FIXED (每次 Whisper ≤15s 单句；current/VAD 每句清空；整体线性、内存恒定).
- Applied minors: (a) clear partial on status stopped/error; (b) guard empty final; (c) clear current during silence.
- P2 backlog additions: move Whisper off consume loop (bounded(256) back-pressure); add a non-ignored VAD segmentation test (commit small fixture/model); short (<1s) utterances skip partial by design; stop_recording still no-op.
- ALL P1.5 DONE. Ready for live smoke + merge.

## P1.6 — SenseVoice recognizer (quality fix: base-int8 Chinese was poor)
User smoke: whisper-base int8 中文识别差、组织不成句。Design doc wanted large-v3; we downgraded for speed. Per-utterance segmentation (P1.5) makes a stronger model feasible. User chose SenseVoice-small (fast, strong zh/en).
- DONE: SenseVoiceRecognizer added (920c8ae), lib.rs switched. Chinese test perfect: '今天开会讨论一下项目进度和下一步计划。'. whisper module kept as alt. Model 347MB fp32 gitignored.
- T: complete (commit 394662e..920c8ae, task review clean/approved by sonnet) — SenseVoiceRecognizer + fetch_models guard + lib.rs swap (whisper kept) + Chinese IT.
  - Model: sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.onnx (347MB fp32; no int8 in tarball; tarball ~1GB not 100MB).
  - Test PASSED: `sense_voice_transcribes_chinese` → "今天开会讨论一下项目进度和下一步计划。" (Tingting TTS, ITN period). All 8 unit tests green; ignored tests model-gated.
  - API: language="auto" worked verbatim; use_itn=true; ctor eyre→anyhow via map_err. No deviations.
  - Minor (deferred, non-blocking): find_model_onnx doesn't try dir.join("model.onnx") first (unlike find_tokens); harmless (one non-int8 onnx in pkg). Mirror find_tokens pattern for determinism.
  - Report: .superpowers/sdd/p16-sensevoice-report.md
  - ALL P1.6 DONE. Live smoke (human): confirm mixed 中英 transcription quality beats whisper-base-int8.

## P2 — 系统声音采集 (branch p2-system-audio)
Plan: docs/superpowers/plans/2026-07-01-voice-notes-p2-system-audio.md
Base (branch start): 144474f
执行模式：subagent-driven；设备验证（T1 探针运行/T3/T8 冒烟）统一推迟到末尾人工冒烟。

### P2 task ledger
- T1: complete (144474f..8c9fbbe, review clean after Co-Authored-By trailer amend) — screencapturekit 依赖 + 系统声音探针; API 校准: get_audio_buffer_list()→audio_buffer_list(), AudioBuffer::data()->&[u8]. Runtime probe deferred to final smoke.
  - Minors (deferred, throwaway report): task-1-report.md AudioBufferList 表缺 buffer() 方法; audio.rs 行号与 spike 文档差 6 行。spike 文档为 T3 权威参考。
- T2: complete (8c9fbbe..0610ea9, review clean, Approved) — Source 枚举 + planar_to_mono; 12/12 tests。无 issue。
- T3: complete (0610ea9..3b827a9, review clean, Approved by opus) — SystemAudioCapture(SCKit)。bytes_to_f32 用 chunks_exact(4)+from_le_bytes(无 unsafe)，4 个真值单测；bg 线程/停止/握手镜像 microphone.rs；错误前缀 unauthorized:/unavailable: 正确；回调对异常缓冲跳过不 panic。16 tests。
  - Minors(deferred): extract_audio_mono 单缓冲 channels==0 走 passthrough(可加 guard，无害)；device 相关 dispatch 无单测(不可测)。
  - 未验证假设(留待末尾冒烟): planar vs interleaved 布局、f32 LE、SCShareableContent::get() 错误一律归类 unauthorized。
- T4: complete (3b827a9..5069591, review clean, Approved by sonnet) — FinalJob/PartialJob + run_segment_worker(识别外置)。finals 双路径(循环+flush)不丢；partial 覆盖式、finals 前清槽。17 tests。run_pipeline 未动。
  - Minors(deferred→final triage): (1) finals_tx.send 用 let _ 吞错，异常关闭时在途 final 静默丢(正常生命周期无碍)；(2) 节流时 current_partial()==None 未清旧槽→过时 partial 可能残留(建议 slot = current_partial().map(...))。
- T5: complete (5069591..cfb408b, review clean, Approved by sonnet, 0 issues) — run_asr_worker。finals 不丢(recv_timeout 缓冲优先于 Disconnected)；"[识别失败]"占位后 worker 续跑；partial take()清槽+错误吞掉；finals 优先。20 tests。
- T6: complete (cfb408b..f937891, review clean, Approved by opus) — start_session + RecordingHandle + SessionStart。死锁自由(每 sender 克隆有确定 drop 点，drop(finals_tx) 后 ASR 见 Disconnected)；stop() 顺序 captures→workers→asr 保证不悬挂不丢；降级 active/failed 正确。22 tests，无 hang。
  - FIX applied (folded into f937891): 实现者曾覆盖共享 fixture sample_16k.wav(真实"Hello"语音→正弦波)破坏 recognizer_it/segmenter_it 语义；已 revert，改用 MockSegmenter(2000) 让 0.42s 短 fixture 在 stop 前产段。
  - Minor(→T7): RecordingHandle 是 Send 非 Sync；T7 须存为 Arc<Mutex<Option<RecordingHandle>>>(plan 已如此)。
- T7: complete (f937891..cd701bc, review clean, Approved by opus) — ipc 加 source/system_audio；lib.rs 双源接线、classify_system(on/denied/unavailable)、就绪后发 recording、真 stop、handle 存 Arc<Mutex<Option>>；删除 run_pipeline+旧测试。build clean，21/21。SystemAudioCapture dead_code 因接线消解(确认已被使用)。
  - Minors(→final triage, 均 plan-inherent): (1) 模型加载窗口内 stop/restart 可孤儿化会话+泄漏线程(UI 在 recording 前禁用 stop→正常不可达；未来 stop-signal 硬化)；(2) 系统 VAD 构建失败会 error→recording 闪一下(罕见，同 vad_path，无害)。
- T8: complete (cd701bc..d8519e6, review clean, Approved by sonnet) — events.ts 加 Source/SystemAudio+source/system_audio；+page.svelte 源徽章(我蓝/对方绿)、两条独立 partial(各自被同源 final 清)、降级横幅(system_audio 有值且!=on)+打开系统设置(openUrl)。check 0 errors，build clean。
  - Minors(→final triage): M1 横幅额外受 status==recording 限(brief 规定；stopped 时 system_audio="" 已排除，belt-and-suspenders 无害)；M2 onMount async unlisten fire-and-forget(pre-existing)。

## P2 全部 8 任务实现完成，待终审(whole-branch) + 末尾人工冒烟

## P2 whole-branch final review (opus): Ready to merge WITH FIXES
- 核心不变量结构正确：clean stop 不丢 final；推理完全在采集回调之外；锁纪律干净、无死锁/泄漏(正常路径)。run_pipeline 无残留。
- Pre-merge fixes:
  - [Important] start_session 未强制 mic 必备：mic 失败但 system 成功 → 静默 system-only 录制、无信号。修复：lib.rs 在 start_session Ok 后断言 Mic∈active，否则 stop+emit error。
  - [Minor#3] segment_worker 节流时 current_partial()==None 未清旧槽(用户可见过时 partial)。一行修复。
- Deferred (记录，未来硬化)：asr 线程 panic 静默丢内容(建议 join Err→emit error)；load 窗口 stop/restart 竞态(建议存 handle 前重查 running)；system-VAD 失败 error→recording 文案；#1/#5/#6 均 defer。
- 冒烟必须确认：(1) SCKit 音频布局(planar/interleaved)+f32 LE+采样率；(2) 权限拒绝→"unauthorized:"→denied、非权限失败→unavailable；(3) 端到端系统声音流+我/对方徽章+打开设置深链。

## P2 pre-merge fixes 复审 (sonnet): Approved
- Fix A(mic 必备) 与 Fix B(清空过时 partial 槽) 均正确、无 scope creep；新增确定性测试 stale_partial_cleared_when_throttle_returns_none。commit 1a9d259，22/22。
- 核实复审的两个"无法从 diff 确认"项：fail() 确实 emit system_audio: String::new()；trailer 已在。均无碍。

## P2 代码完成、全部 review clean。剩：末尾人工冒烟(设备/权限) → 通过后合并。
最终提交序列(branch p2-system-audio, 从 master bb822c6 起)：
  ea349ae 设计 / 144474f 计划 / 8c9fbbe T1 / 0610ea9 T2 / 3b827a9 T3 / 5069591 T4 / cfb408b T5 / f937891 T6 / cd701bc T7 / d8519e6 T8 / 1a9d259 终审修复

## P2 收尾：已推送 origin/p2-system-audio + 开 PR #2
- https://github.com/SoulZhong/voice-notes/pull/2 (base: master)
- 分支保留，未合并。合并前必须先过设备冒烟（PR 描述里 3 项）。

## P2 硬化(延后项 #1): asr 线程 panic 上报 — DONE
- commit 859ee9ee (review clean, Approved by sonnet)。run_asr_worker 两处 recognize 用 catch_unwind(AssertUnwindSafe)：finals panic→"[识别失败]"占位+续跑+eprintln；partial panic→跳过+eprintln；RecordingHandle::stop join Err→eprintln。新增确定性测试 recognize_panic_becomes_placeholder_worker_survives，stderr 干净。23/23。
- Minor(by-design): 测试用全局 panic hook swap(spec 指定)。
- 剩余延后(未做，非阻塞)：load 窗口 stop/restart 竞态；system-VAD 失败 error→recording 文案。

## P2 延后项清理（目标：全部清零）
- e3ba428 (review clean, Approved by opus) 清掉 4 项代码延后项：
  1. [T7] load 窗口停录竞态：load 线程在 running 锁内 check+存 handle；stop 先置 running=false 再取 handle。逐交错验证无死锁无孤儿（单 start/stop）。
  2. [T7] 系统 VAD 构建失败不再发 error 状态：eprintln+跳过 system 源，classify_system 自然给 unavailable。
  3. [T4] finals_tx.send 失败改为 eprintln 日志（两处），不再静默吞。
  4. [T3] extract_audio_mono 单缓冲 channels==0 → 返回空（skip），channels==1 才 passthrough。
- T1 文档两处不一致已修（task-1-report.md 补 buffer() 行、行号对齐 spike 文档 153-195）。
- 无操作项处置（记录理由，关闭）：
  - [T8-M1] 横幅额外受 status==recording 限：brief 明确规定；stopped 时 system_audio="" 本就不显示，冗余但无害。不改。
  - [T8-M2] onMount async unlisten fire-and-forget：Svelte 标准模式、单窗口 Tauri 无害、pre-existing。不改。
  - [T3] extract_audio_mono 设备 dispatch 无单测：需要真设备构造 CMSampleBuffer，不可行；纯子函数(bytes_to_f32/planar_to_mono/to_mono)已有真值测试。接受。
- e3ba428 复审新发现（pre-existing）：快速 start→stop→start 全落加载窗口 → 双加载线程重叠 → 旧 handle 无 stop 被覆盖 → 线程/mic 泄漏。修复中：generation token（fixer 进行中）。
- c26ffa7 (review clean, Approved by opus, 六项交错检查全过) — generation token：AppState.generation；start 入口 bump+捕获 my_gen；stop 先 running=false 再 bump gen；load 线程存 handle/报错前验证 !stale；stale 成功路径 stop 自己的会话、stale fail 只 eprintln 不碰 running 不发事件；mic-mandatory 分支 handle.stop() 先于 fail() 无泄漏。锁序全局一致 running→generation→handle_slot，无死锁。23/23。
  - Minor(cosmetic): fix 报告文字称"同时持三锁"，实际 gen_guard 先释放（更安全），无需改。

## ✅ P2 延后项全部清零 (2026-07-02)
账面延后项(代码4+文档1+无操作3) + 复审新发现(双加载线程泄漏) 全部处置完毕。
分支提交序列新增：859ee9e(asr panic) → e3ba428(4项清理) → c26ffa7(generation token)。
剩余唯一门：末尾人工设备冒烟(SCKit 格式/权限/端到端)，通过后 PR #2 可合并。

## 冒烟中发现并修复：Swift 运行时 rpath 缺失（系统化调试）
- 症状：sckit_probe dyld 崩溃 Library not loaded: @rpath/libswift_Concurrency.dylib。
- 根因：screencapturekit 牌 build.rs 的 cargo:rustc-link-arg rpath 只作用于其自身目标，不传递下游；我们的 test/app 二进制 LC_RPATH 为空。lib 测试没引用 SCStream 符号所以从未暴露；真实 App 同样会崩。
- 修复：src-tauri/build.rs (macOS 门控) emit -Wl,-rpath,/usr/lib/swift。验证：LC_RPATH 出现、探针跑进程序逻辑（停在 TCC 权限门=预期）。lib 23/23 无回归。commit 见 git log。
- 副产品验证：真实 TCC 拒绝确认从 SCShareableContent::get() 报出（NoShareableContent("...TCC")）→ T3 unauthorized: 分类路径正确（冒烟项2部分完成）。

## 冒烟项 1+2 完成（2026-07-02，探针实测）
- 48kHz / PLANAR(2×ch=1×960样本) / f32-LE 全部实锤，extract_audio_mono 走 planar 分支，无需改代码。
- TCC 拒绝 → SCShareableContent::get() 报错 → unauthorized:/denied 分类实锤。
- 静音时 SCKit 持续投递全零 buffer（回调常流，VAD 侧天然过滤）。
- 剩余冒烟项 3（端到端 GUI）：tauri dev + 我/对方徽章 + 两条 partial + 真停止 + 降级横幅 + 深链——需人工操作 GUI。

## 端到端冒烟观察（2026-07-02，用户实测）+ P3 backlog
- 外放视频时 mic 路拾到扬声器声音 → 「我」「对方」同现：声学回声串扰，P2 无 AEC，预期行为。戴耳机即消。
- Backlog[P3+]: AEC——macOS voice-processing AudioUnit (kAudioUnitSubType_VoiceProcessingIO)，cpal 不暴露，需为 mic 路新写 AudioCapture 实现。
- 多人会议「对方」=远端混音单通道，by design；拆分说话人=P3 diarization（设计文档 §4：双路声纹嵌入同池聚类→说话人1..N，source 降为元数据）。diarization 同时缓解回声串扰（同声纹归同说话人）。

## ✅ P2 冒烟全过（2026-07-02，用户实测）→ 合并
- 探针：48kHz/planar/f32-LE/TCC 分类实锤（零代码修改）。
- 端到端：我/对方徽章、双源转写、partial 边说边刷、停止→再开录不用重启，全部正常。
- 回声串扰（外放场景）与多人拆分已明确为 P3 backlog（AEC / diarization）。
- PR #2 squash 合并至 master。

## P3 — 存储与笔记 (branch p3-storage-notes)
Plan: docs/superpowers/plans/2026-07-03-voice-notes-p3-storage-notes.md
Base: 8569ec3
- Task 1: complete (commits 8569ec3..cfe214d, review clean; minor: silero start 透传仅靠审读验证,无独立单测) 
- Task 2: complete (commits cfe214d..4e0181c, review approved; Important[plan-mandated] 时间戳语义注释已对齐 spec §8; minors: segment_worker 递增断言在单 final fixture 下空转、ms 计算块两处重复(沿袭既有模式)) 
- Task 3: complete (commits 4e0181c..3c21f0a, review approved; 注意: store 模块暂有 dead_code warnings(SCHEMA_VERSION/NoteWriter 全家),Task 6 接线后须核对清零; minors: id TOCTOU 假设加注释、file 字段 pub(super) 可收窄) 
- Task 4: complete (commits 3c21f0a..cc099dc, review approved; minors: 多项列表沉底/空行不计数无回归测试、meta.id==目录名为隐式跨模块不变式、list 对 recording 项全量扫 jsonl) 
- Task 5: complete (commits cc099dc..6c3032e, 1 fix round: 半角括号+header 三分支测试+txt 空行结构, re-review approved; minor: md 特殊字符不转义(非需求)) 
- Task 6: complete (commits 6c3032e..350c0c2, opus review approved, 竞态矩阵/锁序核验通过; minor: 停止时不发 storage ok(靠前端 stopped 清横幅), gen/schemas 自动生成文件一并提交) 
- Task 7: complete (commits 350c0c2..327f333, review approved, 迁移逐行核验一致) 
- Task 8: complete (commits 327f333..08878e8, review approved; minors: active 行未禁用删改(靠后端拒绝+横幅)、失败后编辑态不保留、改名+删除并发的理论竞态) 
- Task 9: complete (commits 08878e8..b4f852c, review approved; 偏差 as string 断言经核属必要; minors: $app/stores 为 legacy 路径、h1 改名无键盘入口、durationSecs 依赖 ended_at 隐式不变式) 
- Task 10 (自动化部分): cargo test 40/40, npm check 0 errors(2 a11y warnings 属预期), npm build OK
- 终审 (fable, 全分支): With fixes → 修复 commit 13ea1f0(Critical: recording_status 重建状态; Important: rename 录制中守卫; Important: finalize 失败不置 complete + stopped 清横幅)→ 复审 (opus): Ready to merge YES
- 终审后续项(不阻塞): 加载窗口期新笔记短暂显示已中断且守卫未覆盖; 详情页不过滤空白段; 跨源时间戳非单调(P4 排序); take→finalize 亚秒级改名窗口; $app/stores legacy; h1 改名无键盘入口; 列表 active 行前端未禁用删改
- 待人工冒烟(见计划 Task 10 Step 2 + 新增第 7 条: 录制中→回列表→再进 /record→停止)
