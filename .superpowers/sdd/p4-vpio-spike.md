# P4 Task 1 — VPIO Spike:AEC 可行性验证

**日期:** 2026-07-03
**探针:** `src-tauri/examples/vpio_probe.rs`(保留;粗糙 spike 代码)
**依赖:** `coreaudio-rs = "0.12"`(crate 名 `coreaudio`,macOS target dep)

---

## 结论(TL;DR)

- **可行。** macOS `kAudioUnitSubType_VoiceProcessingIO` 能在 Rust 里创建、初始化并**持续产出麦克风帧**,格式正确、可直喂 SenseVoice。
- **但必须走 `coreaudio-sys` 直调 C API,不能用 `coreaudio-rs` 的高层 `AudioUnit::set_input_callback`。** 后者在本机对 VPIO **和** 普通 HalOutput 输入**都拿不到任何帧**(同一个二进制里 cpal 却正常),是高层封装的坑,详见下文。好消息:`coreaudio-rs` 直接 re-export 了 `coreaudio::sys::*`,所以**仍只需这一个依赖**,Task 8 无需新增 crate。
- **AEC 效果本环境无法验证:** 测试机 `output volume:0, output muted:true`(已用 `osascript` 实锤),`afplay` 外放没有任何声学输出到扬声器,连 cpal 对照侧都录不到泄漏差异(0.039 vs 0.042 RMS,在环境噪声内)。因此可行性结论以「**能拿到 mic 帧 + 格式正确 + 能过 ASR**」为准,AEC 消除量待有外放条件的机器复测。

---

## 实际输出格式(实测)

| 项 | 值 | 备注 |
|---|---|---|
| 子类型 | `kAudioUnitSubType_VoiceProcessingIO` | `kAudioUnitType_Output` / `kAudioUnitManufacturer_Apple` |
| 采样率 | **44100 Hz(设备原生)** | 请求 16000 时 `AudioUnitInitialize` 返回 `-10875`(`kAudioUnitErr_FormatNotSupported`)。VPIO **不做**到 16k 的输入端采样率转换,须下游自己重采样。 |
| 格式 | f32,mono,非交错(flags `0x29` = Float\|Packed\|NonInterleaved) | 在 input 元件(bus 1)的 **output scope** 上设 `kAudioUnitProperty_StreamFormat` |
| 每回调帧数 | 512 帧 | `kAudioDevicePropertyBufferFrameSize`,约 11.6 ms @44.1k |
| 4 秒采集 | 375 次回调,`AudioUnitRender` 成功 375/375,176400 帧 | 无丢帧、无错误码 |

**对 Task 8 的含义:** `VpioMicrophone` 产出的 `AudioFrame { sample_rate: 44100, channels: 1 }`,交给现有下游即可——`audio::resample::resample_linear(&s, 44100, 16000)` 已经存在,和 cpal 路径(48k→16k)完全一致的处理方式。

---

## AEC 效果观察(⚠️ 环境不可验证)

自动化对照:`afplay tests/fixtures/sample_16k.wav`(英文语音,循环放)模拟「对方声音从扬声器出来」,同一 leak 条件下分别用 cpal 与 VPIO 各录 4s:

| 录制 | RMS | dBFS | 说明 |
|---|---|---|---|
| cpal,无 afplay | 0.0417 | -27.6 | 环境噪声基线 |
| cpal,有 afplay | 0.0392 | -28.1 | **与基线无差异 → 扬声器没出声** |
| vpio,无 afplay | 0.0582 | -24.7 | VPIO 基线(含其 AGC 增益) |
| vpio,有 afplay | 0.0441 | -27.1 | 同样无法归因于 afplay |

**根因:** `get volume settings` → `output volume:0, output muted:true`。机器静音,afplay 无声学输出,mic 自然录不到泄漏。cpal 对照侧也证实了这点(有/无 afplay 无差异)。故本机**无法测出 AEC 消除量**。房间本身有人说话(见下 ASR),RMS 在各次运行间波动大(0.003~0.058),也说明测量被环境噪声主导。

> 复测方法(留给有外放条件的机器):取消静音、调高 output volume,重跑
> `( while :; do afplay tests/fixtures/sample_16k.wav; done ) & cargo run --example vpio_probe -- --backend cpal` 与 `--backend raw`,
> 对比英文语音在两侧 wav 的残留 RMS 与 ASR 是否被 VPIO 侧抹掉。

---

## ASR 质量观察(端到端格式正确性 ✅)

把 VPIO 采集(44100→线性重采样 16k)喂 SenseVoice:

```
ASR[vpio]:  就做一些不料的水果。
ASR[cpal]:  然后你那些作为商海文色这一块，那个质量最好。
```

两者是不同时间窗的房间环境语音(非 afplay 的英文,因为静音),内容不可直接对比。**关键结论:VPIO 采到的 f32/44.1k 音频经重采样后被 SenseVoice 正常识别出中文,无格式错乱、无崩溃——端到端管线打通。**

---

## Task 8 推荐路线与关键代码要点

**路线:`coreaudio-sys` 直调 C API(经 `coreaudio::sys` re-export,单依赖)。** 不用 `coreaudio-rs` 的 `AudioUnit::set_input_callback`。

创建/配置顺序(实测有效,见 `record_vpio_raw`):

1. `AudioComponentFindNext` + `AudioComponentInstanceNew`(type=Output, subtype=VoiceProcessingIO, manu=Apple)。
2. `AudioUnitSetProperty(EnableIO, scope=Input, elem=1, 1)`;`EnableIO(scope=Output, elem=0, 0)`(纯采集)。
3. `AudioUnitSetProperty(StreamFormat, scope=Output, elem=1, ASBD)`——ASBD 用**设备原生 44100** f32 mono 非交错(请求 16k 会在 initialize 阶段报 -10875)。
4. `AudioUnitSetProperty(SetInputCallback, scope=Global, elem=0, {inputProc, refCon})`——**必须在 `AudioUnitInitialize` 之前设**。
5. `AudioUnitInitialize` → `AudioOutputUnitStart`。
6. 回调里 `AudioUnitRender(unit, flags, ts, inBusNumber, nFrames, &mut AudioBufferList)`,把 f32 拷进 sink(用 `crossbeam_channel::Sender<AudioFrame>` 配合现有 `AudioCapture` trait)。
7. 停止:`AudioOutputUnitStop` → `AudioUnitUninitialize` → `AudioComponentInstanceDispose`。

握手/线程模型:VPIO 的 IOProc 跑在 CoreAudio 自己的实时线程,不需要像 cpal 那样把 `!Send` stream 塞进后台线程 + ready 通道。`AudioUnit` 句柄是裸指针,`VpioMicrophone` 持有它,`start()` 里建/启、`stop()` 里停/毁即可。refCon 里放 `Sender` + `AudioUnit` 句柄(回调需要句柄来调 `AudioUnitRender`)。

trait 契合:与现有 `microphone.rs` 一样实现 `AudioCapture`,`start(sink)` 非阻塞、`stop()` 释放设备。产出 `AudioFrame { sample_rate: 44100, channels: 1 }`。

---

## 遇到的坑

1. **`coreaudio-rs` 高层 `set_input_callback` 拿不到帧(核心坑)。** VPIO 与 HalOutput 输入两种配置都是「初始化成功、buffer_frame_size=512 正常、但回调零帧」。同一二进制里 cpal 采集正常 → 排除麦克风权限/环境问题。根因推断:`AudioUnit::new` 会立即 `AudioUnitInitialize`,而该封装的 `set_input_callback` 在 **initialize 之后** 才设 `kAudioOutputUnitProperty_SetInputCallback`;VPIO/AUHAL 需要回调在 **initialize 之前** 注册。封装的「init-in-new、callback-later」设计与此冲突,且没有便捷的「设完回调再 init」路径。→ 结论:直调 C API。
2. **VPIO 输入端不接受 16k client 格式。** `SetProperty` 时不报错(返回 0),到 `AudioUnitInitialize` 才 -10875。探针里对 16k/48k/44.1k 逐个试 initialize 才发现;直接用设备原生率 + 下游重采样最稳。
3. **`AudioUnit::new` 立即初始化。** 想改 EnableIO/格式必须先 `uninitialize()`——这是 `coreaudio-rs` 高层路径的额外一步(直调路径天然规避:先配置后 initialize)。
4. **设 `CurrentDevice` 反而搞坏 VPIO。** `VPIO_SET_DEVICE=1` 把 `kAudioOutputUnitProperty_CurrentDevice` 设为默认输入设备后,任何采样率都 initialize 失败。VPIO 用系统默认输入即可,**别显式绑设备**。
5. **测试机静音导致 AEC 无法验证。** 见上;非代码问题,如实记录。

---

## 复现命令(cd src-tauri)

```bash
# 直调 C API 路线(推荐,能拿到帧):
RAW_RATE=44100 cargo run --example vpio_probe -- --backend raw --secs 4 --out /tmp/vpio_raw.wav
# 对照:cpal(正常)/ coreaudio-rs 高层 vpio、hal(均零帧,复现坑 #1)
cargo run --example vpio_probe -- --backend cpal --out /tmp/cpal.wav
cargo run --example vpio_probe -- --backend vpio   # 零帧
cargo run --example vpio_probe -- --backend hal    # 零帧
# 分析:
cargo run --example vpio_probe -- --rms /tmp/vpio_raw.wav
cargo run --example vpio_probe -- --asr /tmp/vpio_raw.wav
```

## 补充(评审修正):线程模型与 Send 约束

上文「不需要像 cpal 那样把 stream 塞后台线程」的说法有误导性:`AudioCapture: Send` 是 trait bound,而 `coreaudio::sys::AudioUnit` 是裸指针、不自动 Send——直接持有会在编译期撞墙。Task 8 二选一:
1. **推荐**:与 `Microphone`(cpal)一致的后台线程模式——AudioUnit 在后台线程创建/启动/停止/销毁,`VpioMicrophone` 只持 stop 通道(天然 Send),ready 握手照抄 microphone.rs。
2. 或者包一层 `struct UnitHandle(AudioUnit)` 并 `unsafe impl Send`,依据:本用例中对该句柄的所有调用(Start/Stop/Uninitialize/Dispose)都串行发生、无并发访问;若选此路须在代码注释里写明该论证。
