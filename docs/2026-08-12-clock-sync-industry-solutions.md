# 多设备/双时钟域音频时钟对齐(clock drift)业界方案调研

> 项目级调研文档。调研时间 2026-08。背景:voice-notes 在 macOS 上双轨录音(mic 经 cpal/CoreAudio,系统声经 ScreenCaptureKit),两条采集管线时钟不同源;现有对账环路用"帧到达墙钟时刻"做测量、容差 1%,晶振级漂移(50–500 ppm)测不出。本文调研专业音频与 WebRTC 的成熟方案,为"手搓 drift correction(硬件时间戳 + 闭环自适应重采样)"提供工程决策依据。
> 来源分级:【官方】一手官方文档(含本机 SDK 头文件注释)/【论文】论文原文/【源码】上游源码原文/【标准】标准文本或标准的权威转述/【厂商】厂商白皮书级技术资料/【媒体】二手报道/【推断】无一手来源的合理推断。关键结论均附来源链接;两篇 Adriaensen 论文与 Drawmer 白皮书为 PDF 原文全文阅读。

## TL;DR

- **专业录音室不存在这个问题,因为他们在硬件层消灭了它**:全系统单一主时钟(word clock / AES11 DARS),所有转换器锁相到同一参考,相对漂移恒等于 0。软件世界的一切方案都是在"回不到单一时钟"时的补救。
- **补救方案只有三族**:① 变比重采样把从时钟"拉"到主时钟(CoreAudio 聚合设备、PipeWire、zita-ajbridge——精度 ppm 级、相位锁定);② 时间伸缩保连续不保相位(WebRTC NetEQ——吸收任意漂移但改变时长);③ 只检测不补偿、靠延迟重估兜底(WebRTC AEC3)。
- **"手搓"的算法核心已有 20 年定论:Adriaensen 二阶 DLL**。三行状态更新(`e = t_read − t1; t1 += b·e + e2; e2 += c·e`),系数 `b=√2·ω, c=ω², ω=2πB/F`。JACK、PipeWire、zita-ajbridge 全是这一族。实测:时间戳抖动压低 100 倍(USB ±2 ms → ±10 µs);闭环控制重采样比后,**比率稳定在 1 ppm 内、相位纹波 <±1.4 µs、约 15 s 收敛**。
- **关键教训(直击本项目痛点)**:DLL 的输入必须是**驱动/硬件给的时间戳**(CoreAudio IOProc 的 `AudioTimeStamp`、SCK 的 CMSampleBuffer PTS),不是"回调到达墙钟时刻";Adriaensen 2012 论文明确指出直接观测缓冲区水位或用回调执行时刻做测量是现有实现(alsa_in)做错的地方。CoreAudio 甚至直接给出了测得的实际速率比(`AudioTimeStamp.mRateScalar`)。
- **对本项目**:路线 A(macOS 14.2+ 私有聚合设备 + CATap,让系统做 drift correction)工程量小、闭环由 OS 维护,但黑盒、有版本门槛;路线 B(自持 DLL + 硬件时间戳 + 变比重采样)全版本可用、可观测可调,精度可达 ppm 级,工程量中等。两条路线不互斥:B 的测量端(DLL 对账)无论如何都值得先做,它同时是 A 的验收工具。

---

## 一、硬件层:字时钟同步(为什么专业录音室没有这个问题)

**结论:数字音频系统的公理是"一套系统只能有一个主时钟"。多台设备互连时,所有设备锁相到同一参考,漂移在硬件层就不存在;软件层的一切都是对这条公理失效时的补偿。**

- **为什么必须单一主时钟**【厂商】:Drawmer 白皮书《Word Clock Synchronization - Why and When You Need It》原文:"Like a band, all digital components need an accurate clock to keep the data stepping through at a constant rate... When two or more digital devices are interconnected, it's important that their clocks be synchronized. **If the clocks get out of step, the receiver may lose or misread a bit, resulting in a click, noise, or muting.**" 以及 "The more devices that a system has, the more important it is to use a single clock source for everything."。来源:[Drawmer clock-sync.pdf](https://www.drawmer.com/uploads/lit/clock-sync.pdf)(PDF 原文已全文核对)
- **AES3 自时钟**【厂商/标准】:同白皮书:"The IEC and AES specifications for digital interfaces require that the receiver have the capability to synchronize its clock to an incoming data stream"——AES3/SPDIF 的双相标记编码天然携带时钟,两台设备点对点即可自同步;**ADAT lightpipe 规范里时钟提取只是可选项**("has provisions for extracting the clock signal from the incoming data stream but it's not required")。
- **AES11 与 DARS**【标准】:AES11(现行 AES11-2020)规范录音棚数字音频设备同步实践,推荐用一路 AES3 信号做全设施时钟分发,称为 DARS(Digital Audio Reference Signal)。来源:[AES11-2020 标准页](https://www.aes.org/publications/standards/search.cfm?docID=18) · [Wikipedia AES11](https://en.wikipedia.org/wiki/AES11)
- **精度分级**【标准/厂商】:Drawmer 白皮书转述 AES11-2003 原文:"defines a Grade 1 clock as one with a long term accuracy of ±1 PPM... A Grade 2 clock has a long term accuracy of ±10 PPM and is generally considered adequate for synchronizing equipment in a single-studio facility."(Grade 1 面向多棚设施)。注意:**这是绝对频率精度;锁到同一参考后设备间相对漂移为 0**,残余的只是 ns 级 jitter。
- **word clock 形态**【厂商/媒体】:采样频率方波,BNC/75Ω 同轴分发,主时钟发生器(如 Drawmer M-Clock)或时钟分配放大器逐台喂给;终端阻抗匹配错误会导致失锁(白皮书有专节)。
- **jitter 量级(锁定质量)**【厂商】:RME SteadyClock 技术文档:常见时钟 jitter 约 5 ns,很好的时钟源 <2 ns;MADI 数据信号因 125 MHz 时间分辨率自带约 80 ns jitter,SteadyClock 最初就是为从 MADI 流中恢复出干净时钟而做,单级锁定、全程高 jitter 抑制,并把清洗后的时钟从 word clock 输出再分发。来源:[RME SteadyClock 技术页(存档)](https://archiv.rme-audio.de/en/support/techinfo/steadyclock.php) · [SteadyClock FS](https://rme-audio.de/steadyclock-fs.html)
- **对照本项目**:Mac 内置 mic 的 ADC 与"系统声"(其实是共享输出设备时钟域的渲染流)分属两个晶振,相当于"两台没接 word clock 的数字设备"——AES 语境下这是不允许直接混录的接法,必须有一方被重采样。

## 二、CoreAudio 聚合设备的 drift correction

**结论:macOS 把"从设备重采样到主时钟"做成了系统能力。聚合设备(Aggregate Device)选一个子设备当时钟源,其余子设备打开 drift compensation 后由 CoreAudio 做变比重采样;质量/CPU 有 0–128 连续档位。macOS 14.2 起系统声进程 tap(CATap)也能作为聚合设备成员,等于"mic + 系统声"可以合成一个单时钟设备。精度与延迟代价无官方数字。**

以下头文件注释均引自本机 SDK `CoreAudio.framework/Headers/AudioHardware.h` 与 `AudioHardwareBase.h`(等同 developer.apple.com 文档正文)【官方】:

- **开关**:`kAudioSubDevicePropertyDriftCompensation = 'drft'`——"A UInt32 where a value of 0 indicates that no drift compensation should be done for this AudioSubDevice and a value of 1 means that it should."
- **质量档**:`kAudioSubDevicePropertyDriftCompensationQuality = 'drfq'`——"A UInt32 that controls the **trade-off between quality and CPU load** in the drift compensation. The range of values is from 0 to 128, where the lower the number, the worse the quality but also the less CPU is used to do the compensation." 预设:Min=0 / Low=0x20 / Medium=0x40 / High=0x60 / Max=0x7F(macOS 13 起更名为 `kAudioAggregateDriftCompensation*Quality` 系列)。**这句话官方承认了实现方式是有质量档位的重采样**。
- **主时钟选择**:组合字典里 `kAudioAggregateDeviceMainSubDeviceKey`(字面量 `"master"`)指定时钟主子设备;`kAudioAggregateDeviceClockDeviceKey`(`"clock"`)可指定独立时钟设备;子设备字典里 `kAudioSubDeviceDriftCompensationKey`(`"drift"`)按设备启用补偿。
- **程序化创建**:`AudioHardwareCreateAggregateDevice` + `kAudioAggregateDeviceIsPrivateKey`(`"private"`,仅创建进程可见,不进系统设备列表)。
- **系统声 tap 入聚合**:`AudioHardwareCreateProcessTap(CATapDescription*, ...)` 标注 `API_AVAILABLE(macos(14.2))`;聚合组合字典支持 `kAudioAggregateDeviceTapListKey`(`"taps"`)把 tap 作为成员,`kAudioAggregateDeviceTapAutoStartKey`(要求同时是 private 聚合)让聚合启动等到被 tap 进程出声。**即 macOS 14.2+ 可以搭一个"mic 子设备 + 系统声 tap"的私有聚合设备,单一 IOProc、单一时钟、drift 补偿由系统闭环。**
- **硬件同域豁免**:`kAudioDevicePropertyClockDomain`——"AudioDevices that have the same value for this property are able to be synchronized in hardware. However, a value of 0 indicates that the clock domain for the device is unspecified and should be assumed to be **separate from every other device's clock domain**."(同域设备不需要 drift 补偿;0 视为各自独立时钟域。)
- **用户侧口径**【官方】:Apple 支持文档《Create an Aggregate Device to combine multiple audio devices》原文:"To set the clock source for the Aggregate Device, choose the device from the Clock Source menu. **Choose the device with the most reliable clock. For each device that is not the clock source, select Drift Correction.**" 来源:[support.apple.com/102171](https://support.apple.com/en-us/102171);Audio MIDI 设置指南进一步把 drift correction 直白解释为 **resampling**("enable drift correction, also known as resampling, to compensate for drift in the data between devices")。来源:[Audio MIDI Setup 指南](https://support.apple.com/guide/audio-midi-setup/combine-audio-devices-a-single-aggregate-ams6e21c3f61/mac)
- **业界横证**【厂商/媒体】:Rogue Amoeba(Loopback/Audio Hijack 作者)知识库:"Audio devices **rarely operate at exactly the sample rate they claim** (e.g. 44099 Hz vs. 44100 Hz)"(44099 vs 44100 ≈ 23 ppm,与晶振级漂移量级一致);并指出 Audio MIDI Setup 只对部分设备自动勾选 drift correction,"For best results, drift correction should be enabled for all devices within an aggregate."。来源:[Rogue Amoeba KB](https://rogueamoeba.com/support/knowledgebase/?showArticle=Loopback-AggregateDeviceHandling)
- **精度与延迟代价**:苹果未发布任何数字。【推断】从"质量 0–128 档的重采样"与聚合设备的环形缓冲结构推断:对齐效果为"样本级连续、无累积漂移"(这是重采样闭环的性质),残余为重采样器相位误差;额外延迟为重采样滤波器群延迟 + 聚合调度缓冲,毫秒级以内,但**无法引用官方数值,需实测**。
- **测量原语(与路线 B 相关)**【官方】:CoreAudio IOProc 回调携带 `AudioTimeStamp`(`inNow`/`inInputTime`/`inOutputTime`),结构体同时含 `mSampleTime`(设备采样帧计数)与 `mHostTime`(`mach_absolute_time` 时基),且 **`mRateScalar`:"The ratio of actual host ticks per sample frame to the nominal host ticks per sample frame"**——系统已经替你测好了"实际速率/标称速率"之比。来源:SDK `CoreAudioBaseTypes.h`、`AudioHardware.h`(`AudioDeviceIOProc` 注释另提醒 `inNow` 含唤醒调度延迟——所以要用 `inInputTime`/`mSampleTime` 对,而不是回调到达时刻)。

## 三、Adriaensen 二阶 DLL:手搓方案的算法核心

两篇论文均已读 PDF 原文:[《Using a DLL to filter time》(LAC 2005)](https://kokkinizita.linuxaudio.org/papers/usingdll.pdf)与[《Controlling adaptive resampling》(LAC 2012)](https://kokkinizita.linuxaudio.org/papers/adapt-resamp.pdf)【论文】。

### 3.1 2005:用 DLL 滤时间戳(测量层)

**问题**:周期回调读系统时钟得到的 (采样计数, 时刻) 映射被五种误差污染——延迟、**抖动**、**时钟量化**、**采样频率误差**("On most hardware, the sample clock is not locked in any way to the one that drives the system timer")与固定延迟。想要的映射必须平滑、单调、连续。

**为什么是二阶**:一阶环只在输入平均速度为零时零稳态误差;而这里输入(时间)以未知速度线性增长(未知 = 真实采样率 ≠ 标称),所以需要**双积分器的二阶环——输入加速度为零即可零稳态误差**,天然跟住未知但恒定的频率偏差。这正是"晶振漂移测不出来"问题的教科书解:漂移变成环路的频率状态量被显式估计出来。

**核心公式(论文式 9–12 + 第 4 节实现)**:每个周期(周期频率 F = 采样率/周期帧数,B 为环路带宽):

```
ω = 2π·B/F          # 归一化带宽
a = 0,  b = √2·ω,  c = ω²   # 临界阻尼

# 每周期迭代(t1 = 预测的下一周期起始时刻,e2 = 频率状态/第二积分器):
e  = read_timer() - t1       # 相位误差:实测 vs 预测
t0 = t1
t1 += b·e + e2               # 相位修正 + 当前周期长度估计
e2 += c·e                    # 频率修正(周期长度自适应 → 这里"住着"ppm 漂移)
```

初始化 `e2 = 周期标称时长; t1 = t0 + e2`;xrun 后重新初始化。估计采样周期 `Te = (t1−t0)/p` 取代标称值,即得连续单调的样本↔时间映射。

**实测精度(论文第 5 节)**:"The DLL... can easily reduce the system time jitter by a factor of 100";USB 声卡时间戳抖动 ±2 ms → 滤后 **±10 µs**;PCI 卡起点更好,"the filtered result will then be better than one microsecond"。

### 3.2 2012:控制自适应重采样(执行层,zita-ajbridge)

**问题定义(摘要原文)**:"Combining audio components that use incoherent sample clocks requires adaptive resampling - the exact ratio of the sample frequencies is not known a priori **and may also drift slowly over time**... 固定比率重采样"will sooner or later require samples to be inserted or dropped and this is in general not acceptable"。并点名 **jack 自带的 alsa_in/alsa_out "None of the currently available solutions... really gets this right"**。

**三条工程要求**:① 比率变化必须小而缓(参照:48 kHz 下 1 个样本 ≈ 空气中 7 mm 声程;高频比率抖动等价于 ADC 时钟 jitter,"some of the existing implementations of adaptive resampling produce this at level that is some orders of magnitude higher than the worst hardware");② 延迟必须稳定可复现;③ 小事故(跳周期)不得改变延迟、不得要求重启。

**方法(论文 3.1–3.3 节)**:
- **不要直接观测缓冲区水位**——两侧并发修改、按周期跳变、且观测时刻(回调实际执行时刻)不代表音频的真实时序("the process callback... can run at any time between the start of the current cycle and the start of the next one")。**这条就是本项目'帧到达墙钟时刻'方案的病根。**
- 正确做法:把读写两侧各自建模为连续函数 W(t)、R(t)——每侧一个 DLL 把 (帧计数, 时间戳) 滤成平滑映射,跨侧用线性插值取同一时刻的值;误差 `E = W(t) − R(t) + d_res − Δ`(d_res 为重采样器内部分数延迟 `inpdist()`,Δ 为目标延迟)。
- E 先过一个**带宽为环路 20 倍的二阶低通**去高频噪声,再进**二阶环路滤波器**(同 2005 DLL 结构)输出重采样比修正;重采样器内部再加一层平滑。
- **收敛加速**:首次测量后对队列做一次硬校正把初始误差直接消掉,随后 4 秒用更高带宽,再降到稳态带宽(**实现中约 0.05 Hz**)。
- 两端标称同频(48.0→48.0)是**最坏情况**:分数误差呈锯齿,必须用 `inpdist()` 修正(本项目正是 48k→48k 场景,此坑直接相关)。

**实测精度(第 5 节 + zita-ajbridge 官方文档)**:1 kHz 正弦经 48.0→44.1 全链路,**约 15 s 收敛;此后相位变化 <0.5° 峰峰 ≈ ±1.4 µs**(1° @1 kHz = 2.78 µs);单核旧机、未打补丁内核。zita-ajbridge 快速指南直接给出口径:"**The resampling ratio will typically be stable within 1 PPM and change only very smoothly.**" 来源:[zita-ajbridge quickguide](https://kokkinizita.linuxaudio.org/linuxaudio/zita-ajbridge-doc/quickguide.html)【官方文档】;重采样器为 [zita-resampler](https://kokkinizita.linuxaudio.org/linuxaudio/zita-resampler-dox/resampler.html)(`Vresampler` 支持连续变比)【源码】。

## 四、PipeWire 与 JACK/ALSA 生态

### 4.1 PipeWire:DLL 是内建原语

**结论:PipeWire 把 Adriaensen 一族的 DLL 做成了 20 行的标准头文件(spa/utils/dll.h),ALSA 驱动节点用它闭环驱动 rate matching;图内所有从时钟设备/流靠自适应重采样对齐到图时钟。**

- **DLL 源码(全文核对,MIT,作者 Wim Taymans)**【源码】:[spa/include/spa/utils/dll.h](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/master/spa/include/spa/utils/dll.h)([GitHub 镜像](https://github.com/PipeWire/pipewire/blob/master/spa/include/spa/utils/dll.h)):

```c
#define SPA_DLL_BW_MAX  0.128
#define SPA_DLL_BW_MIN  0.016

void spa_dll_set_bw(struct spa_dll *dll, double bw, unsigned period, unsigned rate) {
    double w = 2 * M_PI * bw * period / rate;   // 同 Adriaensen 的 ω = 2πB/F
    dll->w0 = 1.0 - exp (-20.0 * w);            // 前置平滑级(≈20×带宽,对应 zita 的预滤波)
    dll->w1 = w * 1.5 / period;
    dll->w2 = w / 1.5;
}
double spa_dll_update(struct spa_dll *dll, double err) {
    dll->z1 += dll->w0 * (dll->w1 * err - dll->z1);  // 平滑后的比例项
    dll->z2 += dll->w0 * (dll->z1 - dll->z2);        // 再平滑
    dll->z3 += dll->w2 * dll->z2;                    // 积分项(频率状态)
    return 1.0 - (dll->z2 + dll->z3);                // 输出:速率修正因子(≈1±ppm 级)
}
```

  带宽常量 0.016–0.128 Hz,与 zita 稳态 0.05 Hz 同量级——**两个独立实现收敛到同一参数区间,这就是 DLL 带宽的工程共识区**。
- **用在哪里**【源码】:ALSA 驱动节点 [spa/plugins/alsa/alsa-pcm.c](https://github.com/PipeWire/pipewire/blob/master/spa/plugins/alsa/alsa-pcm.c) 中 `corr = spa_dll_update(&state->dll, err)` 以缓冲水位/时序误差为输入,输出修正因子写入 `rate_match->rate`(跟随者模式)或换算成硬件 pitch;启动期用 `dll_bw_max`,收敛后降带宽(`spa_dll_set_bw` 两处调用)。
- **官方文档口径**【官方】:[spa_io_rate_match 文档](https://docs.pipewire.org/structspa__io__rate__match.html):"The node can request a correction to the resampling rate in its process()... **Usually the rate is obtained from DLL or other adaptive mechanism that e.g. drives the node buffer fill level toward a specific value.**" 流两端的自适应 sinc 重采样器负责执行任意变比。

### 4.2 JACK / ALSA(简要)

- **JACK 的 DLL**【论文/源码】:2005 论文即为 JACK 引入该机制而写(致谢注明 Florian Schmidt 实现、与 Paul Davis 长期讨论);jack2 中为 [common/JackFilters.h 的 `JackDelayLockedLoop` 类](https://github.com/jackaudio/jack2/blob/develop/common/JackFilters.h)。用途是把周期唤醒时刻滤成平滑的 frame↔microseconds 映射(`jack_get_time`/`jack_frames_to_time`),供客户端与网络后端使用——**测量层,不做重采样**。
- **alsa_in/alsa_out(Torben Hohn)**【源码】:jack 附带的跨声卡桥,[tools/alsa_in.c](https://github.com/jackaudio/jack-example-tools/blob/main/tools/alsa_in.c) 用"平滑窗 + PI 控制器"(`catch_factor=100000`、`smooth_size=256` 加窗平均延迟偏移后积分)驱动 libsamplerate 变比——正是被 Adriaensen 2012 点名"没做对"的前身方案(延迟不稳定、比率噪声大),zita-ajbridge 是其替代品。演进链:alsa_in(PI+平滑)→ zita-ajbridge(双侧 DLL + 显式延迟模型)→ PipeWire(DLL 内建原语)。

## 五、WebRTC 的处理

WebRTC 面对的是"收发两端时钟永不同源"的常态,但它的目标是**通话连续性**而不是**样本对齐**,两条路径(NetEQ、AEC3)都体现这个取向。源码均引自 [webrtc.googlesource.com/src](https://webrtc.googlesource.com/src) main 分支【源码】。

### 5.1 NetEQ:时间伸缩吸收时钟差(不锁相)

- **官方设计文档**【官方文档】:[modules/audio_coding/neteq/g3doc/index.md](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_coding/neteq/g3doc/index.md):NetEQ 是自适应 jitter buffer + 丢包隐藏。`InsertPacket` 统计包到达间隔推导目标播出延迟,且间隔**以 `GetAudio` 的"tick"数计量,"thus clock drift between the sender and receiver can be accounted for"**——即用接收端声卡消费节拍做时间基准,发端快/慢自然反映为缓冲水位趋势;`GetAudio` 将"当前延迟估计(滤波后的缓冲水位)与目标延迟比较",水位过高/过低时对 sync buffer 内容做 time stretch(accelerate / decelerate)。
- **执行机构**【源码】:`modules/audio_coding/neteq/` 下 `Accelerate`(基于基音周期的删除,"The number of samples removed through time-stretching is provided in `length_change_samples`... may remove multiple pitch periods if possible")与 `PreemptiveExpand`(插入拉长),均派生自 `TimeStretch`(WSOLA 族,相关性门限保证接缝无感);缓冲彻底枯竭才走 `Expand`(PLC 外推)+ `Merge`。
- **定性**:NetEQ **不维持相位关系**——它保证播放连续、延迟贴近目标,代价是信号时长被增删(基音周期粒度)。对"双轨录音对齐"这类需要样本级相位保持的场景,这一族方案等于周期性丢/补样本的优雅版,**不可用作对齐手段**,但它证明了"用消费端节拍计量水位趋势"是测量时钟差的有效途径。

### 5.2 AEC3:检测漂移但不补偿

- **缓冲与抖动**【源码】:[echo_canceller3.h](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/echo_canceller3.h) 类注释:EchoCanceller3 "**Partially handles the jitter in the render and capture API call sequence**"——render 侧经带校验的队列缓冲、按 64 样本块与 capture 对齐,吸收的是 API 调用节奏抖动,不是时钟频差。
- **延迟估计**【源码】:[matched_filter.h](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/matched_filter.h) —— MatchedFilter "Produces recursively updated cross-correlation estimates for several signal shifts"(NLMS 自适应互相关);capture/render 先按 `config.delay.down_sampling_factor = 4` 抽取([echo_canceller3_config.h](https://webrtc.googlesource.com/src/+/refs/heads/main/api/audio/echo_canceller3_config.h) 默认值),lag 估计的格点即 4 个样本(16 kHz 下 0.25 ms);[echo_path_delay_estimator.cc](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/echo_path_delay_estimator.cc) 将 lag 聚合为延迟估计。
- **显式漂移检测器**【源码】:[clockdrift_detector.h/.cc](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/clockdrift_detector.cc)——"Detects clockdrift by analyzing the estimated delay":延迟估计每变化一步就与三级历史比对,单调走 ±1/±2(可乱序)判 `kProbable`,凑满 ±3 判 `kVerified`;延迟估计稳定 7500 块(30 s)重置为 `kNone`。**它只输出一个三档标签(用于指标上报),没有任何补偿动作。**
- **补偿去哪了(历史)**【源码】:老一代 AEC(AEC2/AECM 时代)的 `AudioProcessing` 接口有显式漂移补偿,M68 分支 [audio_processing.h](https://webrtc.googlesource.com/src/+/refs/branch-heads/68/modules/audio_processing/include/audio_processing.h) 原文:"**Differences in clock speed on the primary and reverse streams can impact the AEC performance**... This enables a compensation mechanism, and requires that set_stream_drift_samples() be called." + "Sets the difference between the number of samples rendered and captured by the audio devices since the last call to ProcessStream()"(`enable_drift_compensation` / `set_stream_drift_samples`,靠调用方报告两设备的样本数差,内部据此伸缩 far-end 缓冲)。**当前 main 分支的 audio_processing.h 中 "drift" 出现 0 次**(本次 grep 核实)——AEC3 取消了该 API,改为:靠 MatchedFilter 持续重估延迟 + 滞回(`hysteresis_limit_blocks`)吞掉缓慢漂移,每次重对齐后线性滤波器需重新收敛(期间回声抑制靠非线性抑制器兜底)。
- **容忍度量级**【推断(强)】:AEC3 无重采样路径,漂移表现为真实延迟以速率 `ppm × 采样率` 漂走:100 ppm @16 kHz = 1.6 样本/s,即每 ~2.5 s 跨过一个 4 样本估计格点——检测器正是按这个节奏设计的(30 s 稳定窗 vs 步进模式)。漂移越大,重对齐越频繁、线性滤波器越难收敛,回声泄漏越多。"AEC3 大致容忍 ~百 ppm、靠重估而非补偿"是从源码结构得出的推断,WebRTC 未发布官方容忍度数字。

## 六、精度量级对照表

| 方案 | 机制 | 残余漂移/对齐精度 | 收敛/响应 | 代价 | 来源级 |
|---|---|---|---|---|---|
| 硬件字时钟 / AES11 DARS | 全系统锁相到单一参考 | 相对漂移 **0**(样本级硬锁);绝对精度 Grade 1 ±1 ppm / Grade 2 ±10 ppm;jitter 约 2–5 ns | 上电即锁 | 布线/终端阻抗;设备需有 WC/AES 口 | 标准/厂商 |
| CoreAudio 聚合设备 drift correction | 从设备→主时钟变比重采样(OS 闭环) | 官方无数字;性质上无累积漂移、样本级连续【推断】 | 系统管理,不可观测 | 质量档 0–128 换 CPU;重采样群延迟(未公布) | 官方(机制)/推断(精度) |
| Adriaensen DLL + 变比重采样(zita-ajbridge) | 双侧二阶 DLL 建模 + 延迟误差闭环驱动变比 sinc 重采样 | **比率稳定 ~1 ppm 内;相位纹波 <±1.4 µs**;延迟恒定可复现 | ~15 s(硬校正 + 4 s 高带宽) | 单流一个 sinc 重采样器 + 每周期 O(1) 环路运算 | 论文/官方文档 |
| PipeWire spa_dll | 同上,内建原语;带宽 0.016–0.128 Hz | 同族(未发布独立测量) | 启动高带宽→收敛降带宽 | 同上 | 源码 |
| JACK DLL(仅测量层) | 二阶 DLL 滤周期时间戳 | 时间戳抖动 ÷100:USB ±10 µs、PCI <1 µs(不改音频数据) | 数十周期 | 每周期三次乘加 | 论文 |
| WebRTC NetEQ | 消费端 tick 计水位 + 基音粒度时间伸缩 | **不锁相**;吸收任意漂移但改变信号时长;保连续不保对齐 | 水位滤波,秒级 | 时伸缩计算;音质取决于素材 | 官方文档/源码 |
| WebRTC AEC3 | MatchedFilter 重估延迟 + ClockdriftDetector 仅检测 | 对齐格点 4 样本(0.25 ms @16 k);漂移→周期性重对齐+滤波器重收敛 | 检测尺度 30 s | 漂移大时回声泄漏 | 源码 |
| 互相关周期性重锚(本项目现状一族) | 定期用信号互相关硬校正 | 校正瞬间达相关分辨率(样本级),校正间隔内漂移累积;有跳变/重影风险 | 取决于周期 | 相关计算;不连续 | 推断 |

**关键量纲对照**:50–500 ppm 的晶振漂移 = 每分钟 3–30 ms 的累积错位;1% 容差的墙钟对账环路(=10000 ppm)对它天然失明;DLL 闭环后残余是 1 ppm 级 = 每分钟 60 µs 内,且不累积。

## 七、对本项目的落地建议

### 路线 A:macOS 14.2+ 私有聚合设备(mic 子设备 + CATap),系统做 drift correction

组合字典:`{ "subdevices": [mic(主时钟, master)], "taps": [CATap], "private": 1, "tapautostart": 1 }`,非主时钟成员置 `"drift": 1`;单一 IOProc 输出多通道,两轨天然同一时基。

- **优点**:漂移闭环由 OS 维护、零参数;彻底消灭双管线时基问题(连 SCK 的 PTS 口径问题一并消失);代码量小。
- **风险/代价**:`AudioHardwareCreateProcessTap` 门槛 **macOS 14.2+**(SDK 标注),低版本要保留旧路径双栈;tap 需要系统声捕获 TCC 授权(与 SCK 同类但授权面不同,需真机验证);重采样质量/延迟是黑盒(仅 0–128 档位可调,无观测口);出问题(如个别虚拟设备不被自动补偿,Rogue Amoeba 已见先例)难以诊断。
- **工程量**:中——采集层从 SCK 迁到 CATap + 聚合,回放/转写管线不动。

### 路线 B:自持 DLL + 硬件时间戳 + 变比重采样

1. **测量端(无论选哪条路线都先做)**:
   - mic 侧:用 CoreAudio IOProc 的 `inInputTime`(`mSampleTime` + `mHostTime`)喂 DLL;注意 `inNow` 含调度延迟(头文件明示),别用;`mRateScalar` 可直接当测量交叉验证。cpal 若拿不到该时间戳,这就是绕过/补 cpal 的理由。
   - 系统声侧:用 SCK CMSampleBuffer 的 PTS(mach 时基)喂第二个 DLL。已知本项目有"采样率谎报"前科(mic 轨时基漂移结论),DLL 的频率状态 `e2` 恰好就是"真实速率/标称速率"的在线估计,可同时当诊断指标输出。
   - 两个 DLL 都在 host time 时基上,两轨相对比率 = 两条斜率之比;这一步就把 50–500 ppm 从"测不出"变成"连续在线量"。
2. **执行端**:以"轨间累计样本差(经各自 DLL 折算到同一时刻)"为误差,过 20× 带宽预平滑 + 二阶环路,输出变比系数驱动重采样器(Rust 生态:`rubato` 支持流式变比;或按 zita-resampler 思路自带分数延迟读数修正 48k→48k 的锯齿分数误差——本项目正是同标称率场景,这条必须做)。
3. **参数起点**(以 10 ms 周期、F=100 Hz 为例):
   - 稳态带宽 B = **0.05–0.1 Hz**(zita 0.05 / PipeWire 0.016–0.128 的共识区);ω = 2πB/F ≈ 3.1e-3–6.3e-3,b = √2ω,c = ω²。
   - 启动:首次测量后硬校正一次(对齐锚点),随后 **~4 s 用 8–16 倍带宽**(≈0.5–1 Hz)再降到稳态——zita 的收敛配方,总收敛 ~15 s。
   - 平滑:误差预滤波带宽 = 20 × B(二阶低通);比率变化限幅(每周期 ppm 级步进),防止把控制噪声调制进音频。
   - 保护:误差超阈值(如 >5 ms)或 xrun/设备切换 → 重新初始化 DLL 并硬重锚,而不是让环路慢慢爬回。
- **优点**:全 macOS 版本可用;每个中间量(相位误差、频率状态、比率)可日志可回归,评测可复用现有 A/B 工具;精度上限即 zita 口径(ppm 级、µs 级相位)。
- **风险**:参数调优与边界条件(设备热插拔、采样率切换、SCK 时间戳质量)要自己扛;比预期多 2–4 周打磨期;AEC 联动需注意——若未来引入 AEC3,先做完重采样对齐再进 AEC(AEC3 只认 ~百 ppm 内的慢漂移,见第五节)。

### 建议排序

1. 先落 **B 的测量端**(两个 DLL 只测不动数据):工程量小,立刻回答"漂移到底多大、稳不稳定",同时是任何后续方案的验收标尺。
2. 若实测漂移恒定且 macOS 14.2+ 占比可接受 → **A 为主路径**(省一个重采样器和一套调参),B 测量端留作监控;
3. 若需覆盖旧系统、或 A 的黑盒行为实测不达标(延迟不稳/质量档不透明)→ 补全 **B 的执行端**,参数按上文起点。

## 参考来源

**论文(PDF 原文全文阅读)**
- Fons Adriaensen, *Using a DLL to filter time*, LAC 2005 — https://kokkinizita.linuxaudio.org/papers/usingdll.pdf
- Fons Adriaensen, *Controlling adaptive resampling*, LAC 2012 — https://kokkinizita.linuxaudio.org/papers/adapt-resamp.pdf

**官方文档/SDK 头文件**
- 本机 macOS SDK `CoreAudio.framework`:`AudioHardware.h`(drift compensation 属性与质量档、聚合组合键、tap 键、IOProc 时间戳)、`AudioHardwareBase.h`(ClockDomain)、`AudioHardwareTapping.h`(`AudioHardwareCreateProcessTap`, macos 14.2)、`CoreAudioBaseTypes.h`(`AudioTimeStamp.mRateScalar`);网页版同文:https://developer.apple.com/documentation/coreaudio
- Apple Support 102171(聚合设备/Clock Source/Drift Correction)— https://support.apple.com/en-us/102171 ;Audio MIDI Setup 指南 — https://support.apple.com/guide/audio-midi-setup/combine-audio-devices-a-single-aggregate-ams6e21c3f61/mac
- zita-ajbridge quick guide(1 PPM 口径)— https://kokkinizita.linuxaudio.org/linuxaudio/zita-ajbridge-doc/quickguide.html
- PipeWire spa_io_rate_match — https://docs.pipewire.org/structspa__io__rate__match.html
- WebRTC NetEQ g3doc — https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_coding/neteq/g3doc/index.md

**源码(逐文件核对原文)**
- PipeWire `spa/include/spa/utils/dll.h`、`spa/plugins/alsa/alsa-pcm.c` — https://github.com/PipeWire/pipewire
- jack2 `common/JackFilters.h`(JackDelayLockedLoop)— https://github.com/jackaudio/jack2 ;jack-example-tools `tools/alsa_in.c` — https://github.com/jackaudio/jack-example-tools
- WebRTC AEC3:`aec3/echo_canceller3.h`、`aec3/matched_filter.h/.cc`、`aec3/clockdrift_detector.h/.cc`、`aec3/echo_path_delay_estimator.cc`、`api/audio/echo_canceller3_config.h`;NetEQ:`neteq/accelerate.h`、`neteq/preemptive_expand.h`;历史 API:M68 分支 `modules/audio_processing/include/audio_processing.h`(`set_stream_drift_samples`)— https://webrtc.googlesource.com/src

**标准/厂商/媒体**
- AES11-2020 标准页 — https://www.aes.org/publications/standards/search.cfm?docID=18 ;Wikipedia AES11 — https://en.wikipedia.org/wiki/AES11
- Drawmer, *Word Clock Synchronization - Why and When You Need It*(AES11 Grade 1/2 ppm、单主时钟原理;PDF 原文)— https://www.drawmer.com/uploads/lit/clock-sync.pdf
- RME SteadyClock 技术资料 — https://archiv.rme-audio.de/en/support/techinfo/steadyclock.php · https://rme-audio.de/steadyclock-fs.html
- Rogue Amoeba, *Using drift correction to keep aggregate device audio in sync* — https://rogueamoeba.com/support/knowledgebase/?showArticle=Loopback-AggregateDeviceHandling
