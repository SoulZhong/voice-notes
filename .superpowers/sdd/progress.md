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

## P3.5 — UX 重构 (branch p3-storage-notes 续)
Plan: docs/superpowers/plans/2026-07-03-voice-notes-p3.5-ux-rework.md
Base: 569bf30
- P3.5 Task 1: complete (commits 569bf30..51f82bd, 1 fix round: 中间失败路径归还+take 锁窗口, re-review approved) 
- P3.5 Task 2: complete (commits 51f82bd..2a642ff, review approved, 零 findings) 
- P3.5 Task 3: complete (commits 2a642ff..c5281e6, review approved, 全应用监听收敛核验) 
- P3.5 终审 (fable): No→修复 commit 344d0de(Critical: 详情页参数导航不刷新; Important: notesVersion 跨组件同步 / `/` 避开录制中笔记 / 开录 pending 防重+已在录制对账)→ 复审 (opus): Ready to merge YES
- P3.5 后续项(不阻塞): 冷刷新中途录制 finals 不回灌(可用 getNote 水合); 侧栏改名可能吹掉详情页未提交编辑态; 停止后立即开始若遇孤儿线程会冷加载一次; HMR 下 store 重估双监听(仅 dev)
- P3.5 自动化: cargo 41/41, npm check 0 errors, build OK。待人工冒烟(计划 Task 4 Step 2)

## P4 — 说话人区分 + AEC (branch p4-diarization-aec)
Plan: docs/superpowers/plans/2026-07-03-voice-notes-p4-diarization-aec.md
Base: e4c8df2
- P4 Task 1: complete (commits e4c8df2..6cc199d, spike approved; 结论: coreaudio-sys 直调可行, 44.1kHz f32 mono, 回调须 initialize 前注册; AEC 消除量待冒烟机实测; 报告已补 Send 约束澄清) 
- P4 Task 2: complete (commits 6cc199d..86431ff, review approved 零 findings, 真模型测试 PASS) 
- P4 Task 3: complete (commits 86431ff..6999f11, 1 fix round: 合并测试语义如实化+删死代码, re-review approved 经独立仿真核验) 
- P4 Task 4: complete (commits 6999f11..19f9b7b, review approved; 正当偏差: lib.rs 4 处调用点最小适配(None/noop), 真接线留 Task 6) 
- P4 Task 5: complete (commits 19f9b7b..850f94f, review approved; minors: merge 两文件间崩溃窗口(与既有单文件原子哲学一致,建议补注释)、损坏行经重写保留仅靠走查无专测) 
- P4 Task 6: complete (commits 850f94f..af94bcf, opus review approved, 六归还点+死锁面核验; minors: rename 持 session 锁跨 emit(顺序一致无环)、Merged 落盘失败时内存/磁盘瞬时分歧(重启自愈)) 
- P4 Task 7: complete (commits af94bcf..83e8bd6, review approved, 2 minors 已清理) 
- P4 Task 8: complete (commits 83e8bd6..1a181d5, opus review approved, FFI 安全六项核验; 真机自检 44.1kHz 产帧+stop 干净; minors: 回调可加 catch_unwind、teardown OSStatus 未打日志) 
- P4 Task 9 (自动化部分): cargo 55/55 + 真模型嵌入测试 PASS + npm check 0 errors + build OK
- P4 终审 (fable): With fixes → 修复 commit c862f80(Critical: merge 读错误防清空; Important: 合并回写前端 finals / diarization 降级横幅 / rename 单写者 / SpeakersChanged 全量比较; Minor: 预载锁窗)→ 复审 (opus): Ready to merge YES
- P4 后续项(不阻塞): rename 持 session 锁跨磁盘 IO+emit(可收窄); chips 编辑中遇合并可复活孤儿条目; speakerColor 对非 S<n> id 兜底; chips 排序 S10<S2; MERGE_CHECK_INTERVAL 冗余; teardown OSStatus 日志; merge 两文件崩溃窗口注释
- 待人工冒烟(计划 Task 9 Step 2, 六项)+ 聚类阈值校准(Step 3)
- P4 校准 round 1: complete (commit 02f2c8d, 目检通过; ASSIGN 0.62/MERGE 0.74/MIN_NEW 9600 + 短段不拖质心; 依据 18:34 会议实测 10+人只聚 7 簇) 

## P4.5 — 续录 + 回声去重 (branch p4-diarization-aec 续)
Plan: docs/superpowers/plans/2026-07-03-voice-notes-p4.5-resume-echo-dedup.md
Base: d12c665
- P4.5 Task 1: complete (commits d12c665..a370003, review approved 零 findings) 
- P4.5 Task 2: complete (待提交; cargo test 73/73 passed, build 无新 warning; NoteWriter::resume/base_ms + spawn_session(NoteTarget::New/Resume) + resume_recording command; 修复发现的截断尾行追加拼接 bug)
- P4.5 Task 2: complete (commits a370003..f8b7565, opus review approved + 1 fix: abort 目录删除限本会话新建; 附带修复崩溃截断尾行粘行 bug) 
- P4.5 Task 3: complete (commits f8b7565..35e6725, 1 fix round: resuming 全路径复位+预灌注回滚+失败反馈, re-review approved; 后续观察: 已在录制对账分支不回灌真实会话 finals) 
- P4.5 Task 4: complete (commits 35e6725..4660f72, opus review approved; 已知边界: 到期后 system 迟到>hold 则漏网(偏保内容)、短语被包含可能误杀(留二轮校准)) 
- P4.5 Task 5 (自动化部分): cargo 81/81, npm check 0 errors, build OK
- P4.5 终审 (fable): With fixes → 修复 commit 7dc996f(Important: registry_snapshot 空质心项计编号防张冠李戴; Minor: 占位段不参与去重/resume 回滚补 noteId/注释/TS 类型)→ 复审 (sonnet): Ready to merge YES
- P4.5 后续项(不阻塞): 已在录制对账分支不回灌真实会话 finals; 详情页可按 start_ms 稳定排序消除 hold 交错; ECHO 三常量二轮校准
- P4/P4.5 人工冒烟: 用户确认测试通过 (2026-07-04) 

## P5 — v1 收尾 (branch p5-v1-polish)
Plan: docs/superpowers/plans/2026-07-04-voice-notes-p5-v1-polish.md
Base: 2ffb6c3
- Task 0: complete (分支已建, 基线绿: cargo 81 passed / npm check 0 errors 2 已知 a11y warnings)
- Task 1: complete (commits 2ffb6c3..1eb33ee, review clean, Approved by sonnet, spec ✅; 四钉死真值逐字节核对一致, models_dir 零残留, 双入口 guard 在) — models 模块+lib.rs 收敛
  - Minors(→终审 triage): status()/recording_ready() 无直接单测(仅积木有); guard 两入口重复(brief 规定); embedder_it.rs 首行 VN_MODELS 注释是 pre-existing 陈旧文档(代码未读该 env)
- Task 2: complete (commits 1eb33ee..8c39435 + 修复 d54e2da, 1 fix round: extract_and_install 换位安装(备份/回滚)堵 rename 失败毁旧安装窗口, re-review Approved) — 下载器纯逻辑, 6 单测(fixture tar.bz2 现造)
  - Minors(→终审 triage): 回滚分支本身无单测(rename 失败不可移植模拟, 走查核验); d54e2da 把 sdd 文档变更一并提交(squash 后无碍); 报告行数统计两次不准(cosmetic)
- Task 3: complete (commits d54e2da..5d69c87 + 修复 d8300ab/41a1d98, opus review Approved; 偏差: emit 闭包加 move(brief 代码不编译)) — 下载引擎+settings+4 commands
  - 修复链: 审查 Minor「416 满尺寸 .part 永久卡死」→ 首修放 status 分支是死代码(ureq 4xx 走 Err(Status), 控制器自查发现) → 二修挪到 call() 错误分支, 已核 diff
  - Minors(→终审 triage): Content-Length 缺失时 total=offset, UI 可能 >100%(cosmetic); 网络路径无自动化(设计如此, 冒烟覆盖)
- Task 4: complete (commits 41a1d98..bb18638, review Approved by sonnet, spec ✅; set_settings camelCase 映射已核) — 前端下载卡片+录制页集成
  - Minors(→终审 triage, 均 plan-inherent): 下载中离开再回录制页按钮态短暂不一致(点击经「已在进行中」自愈); cancel/toggleMirror invoke 无 catch(失败仅 console); compact 卡片带进度条时的视觉待冒烟目检
- Task 5: complete (commits bb18638..45b4f03, opus review Approved; 两次 API 过载中断经 SendMessage 续跑完成; 门控测试实锤 sherpa flush 中途不重置时间轴; 正当偏差: writer.rs 3 处测试调用点补 None) — 暂停闸+电平回调
  - Minors(→终审 triage): set_paused 暂时 dead-code warning(T6 接线即消, 届时核实清零); 超大帧(>1600 样本)电平只回调一次无余数结转(cosmetic)
- Task 6: complete (commits 45b4f03..ae55453, opus review Approved; 7 处 StatusEvent 构造点全补 elapsed_ms(grep 核), 锁外 emit/幂等/饱和防倒挂全核; T5 dead-code warning 清零(控制器实测 build 仅剩 2 条既有)) — pause/unpause commands+计时+level 事件
- Task 7: complete (commits ae55453..2ab1722, opus review Approved; 七条状态迁移(fresh/resume/pause-unpause/stop-paused/冷刷新x2/双击对账)走查全过, unpause 守卫与 resuming 预灌注不碰撞) — 录制控制条+计时+电平+水合(遗留#7 清)
  - Minors(→终审 triage): isRecording getter 迁移后零消费者(留作 API); record 页重复 import notes 一行(brief 原样); 水合与在途 final 竞态为计划已接受
- Task 8: complete (commits 2ab1722..89b48f6, opus review Approved; serde 回写保真(speaker:null 往返对称)/delete 双查借用/崩溃窗口顺序(先 speakers 后段)全核) — 段落编辑原语+3 commands
  - Minors(→终审 triage): set_segment_speaker 两次 find_seg 重扫(正确仅低效); write_jsonl_atomic 失败留 .tmp(沿既有惯例); reject_if_active 检查与写盘间理论竞态(与 rename_note 同模式)
- Task 9: complete (commits 89b48f6..8c5624c, opus review Approved; 双 $effect 无环证明+三场景走查(改名中编辑保留/id 切换复位/Esc-blur 不双发)全过) — 详情页编辑 UI+遗留#1/#2/#6
  - Minors(→终审 triage): 编辑提交成功路径双 refresh(effect 重跑+显式, 双 getNote 无害); confirmSeq 刻意不在刷新守卫内(brief 规定); speakerMenu/confirm 在途窗口(transient)
- Task 10: complete (commits 8c5624c..276cf79 + 修复 6667948, review Approved + 1 fix round: h1 role="button" 吞标题语义(审查者点出) → 改 h1 内嵌真 button, npm check 0 errors 0 warnings(基线 2 条清零)) — 遗留#3/#4/#5+全量验证
  - 全量验证: cargo test 99 passed / --ignored 7 passed(真模型) / check 0-0 / build OK
  - Minor: speakerColor 修复顺带覆盖 S0 边界(严格超集); h1 单行变多行空白(Svelte 折叠, 冒烟目检)
- 全部 10 任务实现+任务级审查完成, 待全分支终审
- P5 终审 (fable, 全分支): With fixes → 修复 commit 05d6dfd(Important: 时长统一为段落时间轴活跃时长(暂停不计), last_end_ms→max_end_ms 防跨源交错; Minor: start/resume 对账分支补 paused 态)→ 复审 (opus): Ready to merge YES
- 终审 Minors 全部判 defer-OK; 后续项(不阻塞, 记入 backlog): speakers.json 非活动写者(rename_speaker vs set_segment_speaker)加每笔记锁; export 复用详情页 filter+sort 语义; download_running 加 drop-guard 防线程 panic 卡死; 下载中 preload 双载瞬时 2x 内存; 解压阶段取消不即时; 大小卡片切换重建组件态
- P5 自动化终值: cargo test 100 passed + 7 ignored(真模型)全过, npm check 0 errors 0 warnings(基线 2 条清零), build OK
- 待人工冒烟(计划 Task 10 Step 6 六项 + 终审追加): 7) 断网中途→error 可读+重试续传不清零, 镜像无效前缀→错误可理解; 8) 手工造满尺寸 .part→416 路径自愈; 9) 长暂停(≥5min)恢复→VPIO/AEC/系统声音路仍活; 10) 录制中补下声纹→本场降级不变, 停止再开录 diarization=on 无重启; 11) 编辑已中断笔记(删末段/新建说话人)后续录→seq 接续+编号不撞; 12) 冷刷新暂停态→计时冻结正确, 恢复/停止可用

## P5 收尾:已推送 origin/p5-v1-polish + 开 PR #5
- https://github.com/SoulZhong/voice-notes/pull/5 (base: master)
- 分支保留,未合并。合并前必须先过人工冒烟(PR 描述里 12 项)。

## ✅ P5 squash 合并至 master (2026-07-04, commit 7f1a01e, PR #5)
v1 全范围完成。阈值校准(diar/registry.rs + session.rs ECHO 三常量)与终审 defer 项继续挂起,由真实使用反馈驱动。

## P5 backlog 硬化(分支 p5-backlog-hardening,spec/plan: 2026-07-04-*-p5-backlog-hardening*)
- Task 1: complete (commits cb8c830..645dc70, review Approved) — NoteStore 全局编辑锁
  - Minors(→终审 triage): 测试里 Arc<PathBuf> 多余(PathBuf 本可 clone); edit_segment_text 空文本校验前持锁(无害)
- Task 2: complete (commits 645dc70..5f939d2, review Approved) — load 下沉 filter+sort,export/详情页统一
  - Minor(→终审 triage, 计划原文即如此): export 测试 `!txt.contains("00:00:00  \n")` 断言空转(实际三空格),真覆盖在 notes.rs 测试;建议改按段数断言
- Task 3: complete (commits 5f939d2..0de2c7e, review Approved) — download_running RAII drop-guard
- Task 4: complete (commits 0de2c7e..5cd4daf, review Approved) — preload 会话跳过+停录补载(无新单测,计划明示;冒烟④验证)
- Task 5: complete (commits 5cd4daf..6a9dd1f, review Approved; 正当偏差: Artifact 测试构造补 required_for_recording 字段) — 解压 CancelReader+416 复装
  - Minors(→终审 triage): 416 非取消失败包装文案无测试(brief 只要求成功路径); "字节级即时"注释实为按 read 块粒度(措辞)
- Task 6: complete (commits 6a9dd1f..bbd3ae8, review Approved, 真值表核验三态等价) — 下载卡片单实例化
- 全量验证: cargo test 106 passed(新增 6), npm check 0/0, 双端 build OK(2 条 Rust 警告为 master 既有)
- 全分支终审 (fable): Ready to merge YES, 无 Critical/Important; 4 处小修折入(faad920): export 断言实化 / recording.svelte.ts 两处冗余 filter 去除 / CancelReader 注释粒度措辞 / 416 文案不再断言已清理
- 终审 backlog 记录: "cancelled" 字符串契约已有三处耦合点(再增时考虑类型化错误或共享常量); Task 1/5 其余 Minors 判 leave-as-is

## ✅ P5 backlog 硬化 squash 合并至 master (2026-07-04, PR #6)
6 项 defer 全落地 + 常驻编辑态(冒烟反馈)。遗留 backlog: "cancelled" 字符串契约三处耦合(再增改类型化)。

## 语言过滤+RMS(分支 lang-filter-rms)执行
外语(日/韩)幻觉 final 段源头丢弃(标签+字符占比双保险);段级 rms 落盘攒 A1 能量门槛数据。
- spec: docs/superpowers/specs/2026-07-04-voice-notes-lang-filter-rms-design.md
- Task 1: complete (d480ec9..3209da9, review Approved; whisper.rs 补默认为编译连带) — Transcript.lang + is_foreign_final
  - Minor(→T3 收紧): is_foreign_final pub 略宽,按 T3 实际调用方改 pub(crate) 或去 allow(dead_code)
- Task 2: complete (3209da9..51fc6ba, review Approved, 零 issue) — SegmentRecord.rms 贯通
- Task 3: complete (51fc6ba..03d08d1, review Approved, 零 issue; T1 pub 遗留已收紧) — worker 接线:recognize 后取 (text, lang) 过滤外语幻觉段(与 ECHO 命中同待遇,不 embed/不 assign/不 emit/不落盘);段级 rms 经 PendingMic/process_final/on_final 贯通至 lib.rs 落盘;is_foreign_final 收紧为私有、去 allow(dead_code)(T1 遗留 Minor 清理)
- 全量验证: cargo test 109 passed(新增 2), npm check 0/0, 双端 build OK(2 条 Rust 警告为既有 dead_code,与本次改动无关)

## 声纹库(分支 voiceprint-library,叠 lang-filter-rms)执行
- Task 1: complete (457e18f..12529d4, review Approved + 1 fix round: 计划参考代码的 argmax-后验阈值缺口(种子挡道致碎片化)改为候选按各自阈值先滤后选,回归测试锁死) — registry 种子扩展
- Task 2: complete (12529d4..3e05fa2, review Approved, 18 新测) — voiceprints.rs 库模块
  - Minors(→终审 triage): 空 sources 簇静默跳过入库(合理但缺注释); .bak 仅首次覆盖前写、恒久陈旧(按 spec 字面,恢复价值有限)
- Task 3: complete (3e05fa2..96d3bac, review Approved, 5 新测) — person_id 关联 + load 只读 join
  - Minors(→终审 triage): merge_speaker 不传播 person_id(registry winner 继承 + T4 SpeakersChanged 回填可自愈,终审核实); join 每次 load 全量读库文件(v1 可接受)
- Task 4: complete (96d3bac..6c7274e, review Approved; 正当偏差: Option<VoiceprintStore> 替代 clone) — lib.rs 接线+四命令
  - Minors(→终审 triage): store/mod.rs 无消费者的 re-export 用 allow(unused_imports) 压制(应直接删); merge/delete 录制中拒绝有 TOCTOU(手动操作+VP_LOCK 串行,v1 可接受)
- Task 5: complete (6c7274e..b250149, review Approved) — /speakers 管理页+侧栏入口
  - Minor(→终审 triage): 同行合并菜单与删除确认可同时展开(非互斥,cosmetic)
- 全量验证: cargo test 136 passed(声纹库新增 27), npm check 0/0, 双端 build OK
- 全分支终审 (fable): No→修复(170f3ce: Critical 幽灵种子泄漏三处过滤 + Important 种子 count 增量导出防复利 + 4 小项)→复审 Ready to merge YES
- 终审 backlog(记账接受): 库名录制时物化为本地名,此后库改名不传播到这些笔记(需"名字来源"标记才能区分用户改名);speakers.json count 语义变"最近一场净增量",续录质心权重弱化(自限,阈值把守);跨信道命中可能写错库质心槽;续录同人双簇(不够料未入库场景)靠 MERGE/管理页合并逃生;入库前可加相似度挂靠检查(缓解重复未命名人)

## ✅ 三 PR 栈 squash 合入 master (2026-07-05)
- #7 语言过滤+RMS (b1a5ea3) → #10(原 #8 声纹库,基分支删除连带关闭后重开) (091a23e) → #9 设计系统 (e5504d7)。
- 教训: gh pr merge --delete-branch 会连带关闭堆叠 PR 且无法重开——先 gh pr edit --base 再删分支。
- 合并后验证: cargo test 142 passed, npm check 0/0。
