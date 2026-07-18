# Meetily 设计借鉴 + Windows 支持 Implementation Plan

> **For agentic workers:** 借鉴开源产品 meetily 中验证有效的三项工程设计(源级健康统计/断流静音填充、设备断连监控与自愈、跨平台采集),以**融合现有双路独立架构**的方式落地——不搬混音路线,不 copy 代码。同时打通 Windows:系统声音走 WASAPI loopback,全仓可编译。交付在独立分支,**不合 master**,等用户验收。

**Goal:**
1. Windows 上可录「麦克风 + 系统声音」双路并转写(WASAPI loopback 实现 `AudioCapture`),全仓 `cargo check` 通过(Windows target)。
2. 任一采集源中途死亡(设备拔出/蓝牙断电/流错误)不再静默丢一路:UI 收到降级事件,自动重试恢复,时间轴用静音填充保持双轨对齐(借鉴 meetily gap/silence 设计,但在**每源独立**管线里做,混音前提不存在)。
3. 每源管线健康统计(帧数/gap 次数/填充静音时长/重启次数),周期日志 + 查询命令(借鉴 meetily BufferStats)。

**非目标(明确不做):** CoreAudio process tap 替代 SCK(macOS 14.4+ 新依赖,独立后续项);音频文件导入/重转写(独立产品功能);混音/ducking(与双路架构冲突,对比结论已否定);Windows 打包签名与真机验收(本机为 macOS,只能做到交叉 check,见「验证边界」)。

**Architecture(融合点,全部沿既有惯用法):**
- `audio/loopback.rs`(新,仅 Windows 编译):cpal 对默认**输出**设备建 input stream = WASAPI loopback,线程持流 + stop_tx/ready_rx 握手,与 `microphone.rs`/`system.rs` 同构。
- `pipeline/frame_tap.rs`(新,平台无关):**每源一个转发级**,插在 capture 与 segment_worker 之间(`capture → tap → worker`)。职责三合一:
  1. **健康统计**:帧/样本计数、到达间隔 gap 检测、统计快照(`Arc<SourceHealth>`)。
  2. **断流静音填充**:`recv_timeout` 检测帧荒(阈值按源配置),按墙钟差补零帧——保双轨时间轴对齐(WASAPI loopback 无播放时不回调,这是 Windows 正确性的**必要**件,不只是鲁棒性)。
  3. **失联上报**:帧荒超阈值/收到流错误事件时通知会话监控。
- `CaptureEvent` 错误通道:cpal 系 capture(mic/loopback)的 `err_fn` 从裸 `eprintln` 改为可选上报;`ResilientCapture` 包装器(实现 `AudioCapture`)持 factory 重建内层采集,**复用同一 sink sender**——worker 不知情不重启,时间轴由 tap 的静音填充接续。有界退避重试,恢复/放弃都发 ipc 事件。
- lib.rs 装配:System 源 Windows 分支用 loopback;Windows 无 VPIO,mic+system 齐备时**恒走软件 AEC 链**(等价于 macOS 的 keep_output_volume 模式);AEC 不可用时降级无 AEC(文本级回声去重链兜底,平台无关已具备)。
- 跨平台清障:`notelock` 改 std `File::try_lock`(rustc 1.89 已稳定,unix 仍是 flock 语义);`logging` 用 `std::io::IsTerminal` 判 tty、dup2 重定向 cfg(unix)(Windows 首版跳过黑匣子,记为已知限制);`transcode` Windows 直接保留 WAV(afconvert 不存在,显式 early-return 替代反复报错)。

## Global Constraints

- **不 copy meetily 代码**:借鉴的是设计(gap 检测/静音填充/统计口径/断连自愈),实现全部按本仓惯用法重写(crossbeam 通道、stop_tx drop 语义、中文注释写「为什么」)。
- **双路独立红线**:任何改动不得引入混音;tap 是逐源的,静音填充是为时间轴对齐,不是为合流。
- **现状不回退**:macOS 默认路径(VPIO)行为不变;tap 引入后 macOS 全部既有测试必须绿;AEC/统计/自愈任何失败都降级到现状行为,绝不挡录制。
- **验证边界(诚实声明)**:本机 darwin。Windows 侧能做到:`cargo check --target x86_64-pc-windows-msvc`(rustup target + 原生依赖 build script 能否跑完取决于依赖,见 Task 1 探路);做不到:真机录音/回放/托盘目检。计划里所有 Windows 行为断言以「待真机验收」标注。
- **分支** `feature/meetily-borrow-windows`(从 master);`git add` 显式路径禁 `-A`;提交无署名尾注;**不合 master**。
- 平台差异全部收敛在 lib.rs 装配层与 cfg 模块声明,核心管线(session/segment_worker/asr/diar)保持平台无关。

## 依赖调研结论(2026-07-18 已查证,出处见调研记录)

- **cpal 0.15 WASAPI loopback:可用**(0.13.1 起,PR #478)。用法:`default_output_device()` + **`default_output_config()`**(对输出设备取 input config 会 `StreamTypeNotSupported`)→ `build_input_stream`,WASAPI 端对 eRender 设备自动加 `AUDCLNT_STREAMFLAGS_LOOPBACK`。格式通常 f32/48k/2ch。**实锤:无音频播放时回调根本不触发**(swyh-rs 以"InjectSilence"补偿同款行为)→ FrameTap 静音填充是正确性必需件。要求 Win10 1703+(event-driven loopback);COM 冲突规避:建流放独立线程(本仓惯用法本来如此)。
- **webrtc-audio-processing 2.1:Windows 不可构建**。官方 CI 无 windows;issue #34 维护者明言无测试机、等社区;build.rs 走 meson+pkg-config+`-std=c++17`(GCC 语法),MSVC 必挂,无可用 fork。→ **定案路线 B**:Cargo.toml 圈 `cfg(not(windows))`,`audio/aec.rs` 出 Windows stub(同形 API,构造返回 Err → 既有「无 AEC 降级」路径),文本级回声去重为 Windows 兜底;实时 DTLN-aec(tract 纯 Rust,本可跨平台)记为后续增强。
- **sherpa-rs 0.6(download-binaries)/ tract-onnx 0.21:Windows 正常**(sherpa 需 VS2022,官方明确不支持 MinGW)。
- **macOS 宿主交叉 `cargo check`:不可行**(本机实测 ring 的 cc 步骤即挂 `assert.h`;webrtc-ap 的 meson 亦无交叉环境)。→ **真验证改走 GitHub Actions `windows-latest` check job**(Task 6),本机只保证 macOS 全绿 + 平台无关逻辑单测覆盖。
- 顺带发现(后续项,不入本分支):cpal 0.17+ 新增 **CoreAudio loopback(macOS 14.6+)**,有望同时替代 SCK(免屏幕录制权限)与 WASAPI 两侧的自研采集——升级 cpal 有 breaking changes,独立评估。

## Task 1: 跨平台清障(notelock/logging/transcode)+ Windows target 探路
**Files:** `store/notelock.rs`、`logging.rs`、`store/transcode.rs`
- notelock:`libc::flock` → `File::try_lock()`/`TryLockError`(std,跨平台;unix 底层仍 flock,语义不变),既有并发单测原样通过。
- logging:tty 判定改 `std::io::IsTerminal`;dup2 重定向包 `#[cfg(unix)]`;Windows 分支写明「GUI 无黑匣子,dev 控制台仍可见」+ 注释记后续(SetStdHandle 路线)。
- transcode:`transcode_note_dir`/`decode_note_to_wav` 开头 `#[cfg(windows)]` 直接 return(保留 WAV 即最终态),避免每场录音刷「转码失败」日志;`track_pcm` 已优先读 WAV,精修链天然可用。
- 探路(已完成):交叉 check 在 ring 的 cc 步骤即挂,结论回填上方;真验证改 CI(Task 6)。
- macOS `cargo test` 全绿后提交。

## Task 2: FrameTap(统计 + 静音填充 + 失联检测,纯逻辑先行)
**Files:** Create `pipeline/frame_tap.rs`;`pipeline/mod.rs`
- `SourceHealth`(Arc 内原子字段):frames、samples、gaps、silence_inserted_ms、restarts、last_frame_unix_ms;`snapshot()` 出可序列化结构。
- `run_frame_tap(source, from_capture_rx, to_worker_tx, health, policy, on_stall)`:转发帧并计数;`recv_timeout(tick)` 无帧时按墙钟差生成零帧(带上一帧的 sample_rate/channels;从未收过帧则不填充——源本来就没起来);帧荒超 `stall_after` 调 `on_stall`(去抖,恢复后可再触发)。
- policy 按源:Mic `fill_after=500ms`(正常麦克风静音也持续出帧,帧荒=设备异常);System(Windows loopback)`fill_after=250ms`(无播放即无回调,填充属常态);System(macOS SCK)`fill_after=1s`(SCK 静音也回调,帧荒罕见)。数值常量集中放顶,注释写依据。
- 单测(mock 通道,不碰设备):转发保序、计数正确;断流后补零量≈墙钟差;stall 回调触发与去抖;从未收帧不填充;关闭上游 → 排干 → 关闭下游。

## Task 3: 接线 FrameTap + 会话级健康暴露
**Files:** `session.rs`(start_session 源循环)、`lib.rs`(新 tauri command)、`ipc.rs`
- start_session 每源:`capture → tap_rx`,起 tap 线程转发到原 `ftx`;`SessionStart` 增每源 `Arc<SourceHealth>`(会话槽持有)。
- 新命令 `pipeline_health`:录制中返回各源快照 JSON,未录制返回空;周期日志(tap 内每 ~30s eprintln 一行摘要,静默场次零输出干扰)。
- 既有测试全绿(tap 对 mock 流透明);新增一条 start_session 级集成测试(Mock capture 断流 → health.gaps>0)。

## Task 4: 设备断连自愈(CaptureEvent + ResilientCapture)
**Files:** `audio/mod.rs`(CaptureEvent)、`audio/microphone.rs`、新 `audio/resilient.rs`、`session.rs`/`lib.rs` 接线、`ipc.rs`(降级事件)
- `Microphone::with_events(tx)`:err_fn 上报 `CaptureEvent::Error(String)`(Windows loopback 同款;VPIO/SCK 首版不接,注明其错误面(权限/内部失败)已由启动期分类覆盖,运行期死亡由 tap 帧荒兜底——两条探测路径互补)。
- `ResilientCapture`:包 factory(`Fn() -> Box<dyn AudioCapture>` + 事件接收端),start 时留存 sink clone;监控线程收到 Error 或 tap stall 信号 → stop 内层 → 退避重试(1s/2s/4s,封顶 3 次一轮,恢复后计数清零)→ 重启成功发 `source_recovered`、放弃发 `source_lost`(ipc StatusEvent 扩展或独立事件,前端横幅消费——本计划只发事件+现有横幅字段,不动前端布局)。
- 时间轴:重试窗口内 tap 已在补静音,恢复后帧续上,双轨对齐不断裂(单测:mock factory 第一次死第二次活,验证 sink 连续性与 restarts 计数)。
- macOS 手工冒烟:录音中拔掉 USB 麦克风/断开蓝牙麦,观察事件与恢复(本机可验)。

## Task 5: Windows 系统声采集(WASAPI loopback)+ 装配
**Files:** Create `audio/loopback.rs`(cfg windows)、`audio/mod.rs`、`lib.rs` 装配区
- `LoopbackCapture`:cpal default_output_device → 按调研确认的 API 对输出设备建 input stream;格式除 f32 外接受 i16(转 f32,loopback 常见);线程/握手/停止与 microphone.rs 同构;错误分类沿 `unavailable:` 前缀(Windows 无授权概念,`classify_system` 无需改)。
- lib.rs:System 源三分支(macOS→SCK;Windows→Loopback;其余→不建);Windows 下 `keep_output_volume` 语义=恒真(无 VPIO 可选),AEC 装配条件改为 `(keep_output_volume || cfg!(windows))`,按 Task 1 调研结论门控 aec/echo_clean 模块链。
- `cargo check --target x86_64-pc-windows-msvc` 通过(或记录依赖阻碍与绕行);macOS 全测试绿。

## Task 6: 收尾与验收材料
- 全仓 `cargo test` 绿;`cargo check` Windows target 结果留档;macOS 真机冒烟:正常录一段(VPIO 路径不回退)+ 拔设备自愈演示。
- 更新本计划「依赖调研结论」与各任务偏差记录;写验收说明(用户可验项/待 Windows 真机项清单)。

## 实施偏差记录(2026-07-18)

- **Tap 不进 start_session,改 TappedCapture 包装器**:原计划在 start_session 源循环里插 tap;实施发现那会把填充语义暴露给全部既有 Mock 流测试、并把平台策略渗进 session 层。改为装配层包装(`TappedCapture(ResilientCapture(真实采集))`),session.rs **零改动**,既有 527 测试原样绿。
- **周期日志改事件驱动**:原计划"每 ~30s 一行摘要";实施改为断流/恢复/自愈结局时才落日志——Windows 环回的静默填充是常态,周期日志会刷屏,事件驱动才符合"静默场次零输出干扰"的本意。统计随时可查(pipeline_health)。
- **SCK 失联阈值**:原计划 System 不判失联;定稿为 SCK 5s 判失联(它静音也持续回调,帧荒即流死亡)、仅 Windows 环回不判(静默=常态)。
- **CI 首轮实收 5 个 Windows 编译错**(正是要真 runner 的原因):mcp uds/bridge 的 Unix socket(std 不在 Windows 暴露)→ #[path] 桩顶替,控制类 MCP 降级人话指引、查询类不受影响;`RunEvent::Reopen` 是 macOS 独有变体 → cfg 圈起。
- **测试竞态教训 ×2**:重建实例的帧在 start() 内同步发出,on_recovered 在 start() 返回后才调——"收满帧"不代表"回调已执行",两处断言都改为有界轮询。

## 验收清单(交付用户)

**本机(macOS)已验:**
- `cargo test --lib` 528 通过 0 失败(frame_tap 7 项、resilient 4 项、组合全链 1 项为新增;flaky 排查:frame_tap 15/15、resilient 20/20 轮次稳定)。
- 真设备 ignored 测试:VPIO 真麦克风出帧 ✓;DTLN 双模型加载/守恒/静默参考 3/3 ✓(`VN_DTLN_DIR` 指向已装模型)。
- 前端 `npm run check` 0/0、`npm test` 7/7。
- Windows 编译:GitHub Actions windows-latest `cargo check --lib`(分支每次 push 自动跑)。

**待用户验收(需人工/真机):**
- macOS 整机:开录正常一场(默认 VPIO 路径不回退)→ 录制中拔 USB/蓝牙麦克风 → 观察 stderr 自愈日志 + `source_health` 事件 + 恢复后转写继续、时间轴不错位;`pipeline_health` 命令可查各源计数。
- Windows 真机(本机无以验证):双源录制、环回静默期时间轴、设备切换自愈、sherpa 预编译包下载路径。
- 决定是否合并:分支 `feature/meetily-borrow-windows`,未合 master。

## Self-Review
- 三项借鉴(统计/静音填充+断连自愈/跨平台采集)全部落在双路架构内,无混音渗入 ✓
- meetily 的坑已规避:统计不是死代码(接线+命令)、静音填充有单测口径、错误面有两条互补探测 ✓
- Windows:采集/装配/清障/AEC 门控成链,验证边界诚实 ✓
- macOS 现状零回退(VPIO 默认路径不动,tap 透明,失败全降级)✓
