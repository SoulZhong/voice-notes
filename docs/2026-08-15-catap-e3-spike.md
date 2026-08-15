# E3 spike:CATap + 私有聚合设备(路线 A)实测报告

> issue #99 二期的 E3。判据在 `docs/2026-08-12-clock-drift-sensor-design.md` 第五节,
> 预注册,本文只填数字不改判据。工具:`src-tauri/examples/catap_spike.rs`。
>
> **本文只是探针结论,路线 A 尚未裁决通过,采集主干一行未动**——把未验证的采集路径
> 合进主干,风险大于收益(issue #99 留言已立此规矩)。

## 一句话现状

路线 A 在本机(macOS 26.5.2)**技术上完全跑得通**:私有聚合设备 + 全局 CATap,
单 IOProc 同时出 mic 与系统输出两轨——**每次回调两轨帧数都相等**(8449 次回调零例外)、
**6 分钟零时间戳断层**;开销:IOProc 回调占用 0.05~0.07% 单核、本进程采集期 CPU
0.17~0.19% 单核,**coreaudiod 侧的 tap/聚合/重采样开销未测**(那才是路线 A 的主要成本项)。

跨时钟域(内置麦 master + 蓝牙输出)的参考对照斜率 -3.26ppm,与 master 自身的时钟误差
(-3.1~-3.7ppm)吻合,**与"补偿生效"的预期一致**。但这不构成对残余漂移的定量上界:
播放侧设备的时钟误差没被独立量过(见下节),两块晶振恰好接近时,即使补偿没生效也会
是这个读数。

**裁决不能下**,缺四块:判据 1 的声学口径(要"内置麦 + 外放"的场,本机当前输出在蓝牙
耳机上做不了)、播放侧时钟误差的独立测量、判据 3 热插拔完全未测、判据 4 只测了终端
身份没测签名 app。另外走 A 就要为 13.x–14.1 维护双栈采集路径,这笔成本要与 B
(DLL 闭环 + rubato 变比重采样)的实现成本一起算。

## 工具

```bash
cd src-tauri
cargo run --example catap_spike -- list      # 列输入设备 + uid
cargo run --example catap_spike -- probe     # 能力/授权探针:只建拆设备,不落音频
cargo run --example catap_spike -- capture --secs 370 \
    --input-uid BuiltInMicrophoneDevice --out /tmp/catap
cargo run --example catap_spike -- capture --secs 370 --native --out /tmp/catap  # 原始率落盘
```

`capture` 默认落两条 16k mono s16 轨(`mic.wav` / `system.wav`),与 app 的规范轨同格式,
`bin/xcorr_align` 可直接读。

`--native` 按设备原始率(48k)落盘。**与外部参考文件比对时必须加这个**:探针里的
48k→16k 是无抗混叠的线性抽取,宽带内容(白噪)一混叠,波形相关性就没了——第一次参考
对照实测 corr 只剩 0.10,就是栽在这上面。轨间(mic vs system)比对不受影响,因为两轨
走同一条抽取、误差同相抵消。

## probe 实测(本机 macOS 26.5.2)

| 项 | 结果 |
| --- | --- |
| `AudioHardwareCreateProcessTap` | 有(dlsym 取,coreaudio-sys 0.2.18 的绑定里没有这个符号) |
| `CATapDescription` 类 | 有 |
| 建 tap 耗时 | 2.8ms |
| tap 格式 | 48000Hz 2ch float32 |
| 建私有聚合设备耗时 | 34–47ms |
| 输入通道布局 | `[1, 2]` = mic 1ch + tap 2ch,顺序就是描述里的 subdevices→taps |
| 聚合设备标称率 | 恒等于 master(mic)的标称率:内置麦 48000Hz / 蓝牙 HFP 16000Hz |
| 输入延迟 | 内置麦 26 帧(0.5ms);蓝牙 HFP 160 帧(10.0ms) |
| 安全偏移 | 内置麦 998 帧(20.8ms);蓝牙 HFP 181 帧(11.3ms) |
| 私有性 | 采集进行中,该聚合设备**不出现**在设备枚举里,系统声音设置里也看不到 |

### 踩坑:selector 名猜错 = 进程 abort

第一版按直觉发 `setPrivateTap:`,结果 ObjC 抛 unrecognized selector,而 **Rust 接不住
外来异常,整个进程直接 abort**(`fatal runtime error: Rust cannot catch foreign exceptions`)。
CATapDescription 没有公开头文件可查版本差异,所以探针改成先问运行时要方法表再发:

```
UUID  bundleIDs  deviceUID  init  initExcludingProcesses:andDeviceUID:withStream:
initMonoGlobalTapButExcludeProcesses:  initMonoMixdownOfProcesses:  initMonoMixdownOfProcessesIDs:
initStereoGlobalTapButExcludeProcesses:  initStereoMixdownOfProcesses:  initStereoMixdownOfProcessesIDs:
initWithDictionary:  initWithProcesses:andDeviceUID:withStream:  isExclusive  isMixdown  isMono
isMuted  isPrivate  isProcessRestoreEnabled  name  processes  setBundleIDs:  setDeviceUID:
setExclusive:  setIsExclusive:  setMixdown:  setMono:  setMuteBehavior:  setName:  setPrivate:
setProcessRestoreEnabled:  setProcesses:  setStream:  setUUID:  stream
```

两个与直觉不符的事实:**私有属性的 setter 是 `setPrivate:`(不是 `setPrivateTap:`)**;
**没有 `initExcludingProcesses:` 的短形式**,只有带 `andDeviceUID:withStream:` 的长形式。
所以产品化时凡是发可选 selector,一律先 `respondsToSelector:`。

## capture 实测(370s,内置麦为 master + 蓝牙耳机为输出)

设备组合是刻意选的:master 是内置麦(48k),被 tap 的输出走蓝牙耳机——**两个独立硬件
时钟域**,是路线 A 最难的场景。若换成内置麦 + 内置扬声器,两者同一时钟域,测了也不说明问题。

跑了两场 370s(第二场把参考换成带限噪声):

| 项 | 第一场 370s | 第二场 370s | 补测 60s | 补测 30s |
| --- | --- | --- | --- | --- |
| 回调 / 帧 | 34700 / 17.77M | 34684 / 17.76M | 5627 / 2.88M | 2822 / 1.44M |
| **两轨累计样本数差** | **0** | **0** | **0** | **0** |
| **逐回调两轨帧数不等** | (未测) | (未测) | **0 次** | **0 次** |
| **时间戳断层** | **0 次** | **0 次** | **0 次** | **0 次** |
| 回调耗时 p50/p95/max | 3/24/120 µs | 3/24/225 µs | 3/27/124 µs | 3/26/60 µs |
| 回调占用(墙钟) | 单核 0.05% | 单核 0.05% | 单核 0.06% | 单核 0.06% |
| 本进程 CPU(采集期增量) | (未测) | (未测) | (口径未净化) | **单核 0.17%** |
| 聚合设备时钟 vs `Instant` | (口径有 bug,见下) | **-3.7ppm** | -5.8ppm | -14.3ppm |
| 轨道电平 | mic .019 / sys .064 | mic .013 / sys .062 | mic .008 / sys .009 | mic .012 / **sys 0** |

三条读表的注意事项:

- **逐回调对账是后加的**(Codex 审查指出:只比最终累计长度撑不住"每次回调都等长"这个
  说法——某次少给、下次补回来,累计照样相等)。两场 370s 跑在加仪表之前,只有累计数;
  补测的 60s/30s 两场逐回调核过,8449 次回调**一次不等都没有**。
- **"聚合设备时钟 vs Instant"要长跑才准**:首/末回调的时刻各有一个缓冲块(512 帧 ≈ 10.7ms)
  的粒度误差,30s 跑上就是几百 ppm 的分辨率下限——所以 30s 那栏的 -14.3ppm 不说明什么,
  只有 370s 那场的 -3.7ppm 可用。
- 最后一栏 **system rms 恰为 0**:那次没放任何音频。这反过来证明前几场 system 轨里的
  信号确实来自 tap 抓到的播放内容,不是底噪或串扰。

**"回调占用"不等于 CPU 成本**(Codex 审查指出):它只是本进程 IOProc 内部的墙钟耗时,
既不含 coreaudiod 里 tap / 聚合设备 / drift 重采样的开销,又混进了回调被抢占的时间。
探针已改为同时打印 `getrusage`,并且取的是**采集期增量**(起流前后各一次快照)——
第一版把快照放在落盘之后,把写两条几分钟长 WAV 的开销算了进去,0.06% 报成了 0.67%;
只取一次绝对值也不行,进程启动那几百毫秒会把短跑的数字抬高好几倍。
即便如此,**系统侧开销仍未测**——真要给路线 A 的 CPU 成本下结论,得另外量 coreaudiod。

第一场的"实测源域率"打成了 -154.6ppm,是探针自己的口径 bug:分母用了
`AudioDeviceStart → 停止` 的整段墙钟,两头各混进一截与采样无关的时间(设备启动延迟
~0.2s、主循环 200ms 轮询粒度)。改成"首回调 → 末回调"之后读数就落回 -3.7ppm,
与传感器口径吻合。**教训:任何"实测速率"都要先说清分母是哪两点之间。**

"两轨帧数相等"不是巧合,是路线 A 的**结构性保证**:单 IOProc 一次回调同时给出两轨,
帧数天然相同(补测里 8449 次回调逐次核对,零例外)。这正是它相对现状(mic 走 cpal、
system 走 SCK,两条独立时钟各自交付)的根本优势。

### 一个反面观察:蓝牙耳机做 master 时 mic 会中途哑掉

15s 冒烟那次用 Shokz(HFP 16k)当 master,前 6 秒 mic 有声、之后掉到 0.0003 的静音底噪,
而 system 轨全程正常。原因是播放触发了 A2DP/HFP 的 profile 切换。这与路线 A 无关
(现状架构同样会遇到),但说明**蓝牙组合的 E2 数据要单独看**,不能与内置麦混在一个分布里。

## 残余漂移:两种量法,别混为一谈

判据 1 说的是**轨间残余错位**。要理解它必须先分清两个量:

1. **轨间错位(判据 1 本体)**:同一个真实声音,在 mic 轨与 system 轨里的样本位置之差随
   时间如何变化。mic 就是 master 时钟本身,所以这一项 = tap 被 drift 补偿之后**相对 master
   时间轴**的残余速率误差。量法:让 mic 真的听见播放内容(外放),
   `xcorr_align system.wav mic.wav` 的斜率。**master 自身的时钟误差在这里是共模,自动抵消**。
2. **参考对照(本次做的)**:把播放用的参考文件与 system 轨互相关。它 =(补偿残余)+
   (master 时钟相对真实时间的误差)+(播放侧设备时钟误差)。
   **它不是补偿残余的上界**——各项有正负,理论上可以互相抵消掉一个更大的残余。
   要从它反推残余,只能"减去独立测得的共模项",而这个减法的可信度取决于那些独立测量
   自身的不确定度。判据 1 本身要用下面的声学口径量。

本次跑的是第 2 种(本机输出在蓝牙耳机上,内置麦听不见它,做不了第 1 种)。

### 参考对照实测(370s,内置麦 master + 蓝牙输出)

播一段 380s 的带限噪声(≤1.5kHz),与采集到的 system 轨逐窗归一化互相关、最小二乘拟合:

```
窗数 37 / 斜率 -3.26ppm = -11.7ms/小时 / 拟合残差 rms 0.021ms max|resid| 0.042ms
整段错位变化: 首窗 +0.062ms → 末窗 -1.062ms (跨度 360s)
```

**关键在于这 -3.26ppm 是什么。** 四个独立来源交叉验证:

| 来源 | 值 |
| --- | --- |
| 本次参考对照斜率(37 窗全用) | **-3.26 ppm** |
| 同上,只用 corr>0.5 的 6 窗 | -3.36 ppm |
| 本次聚合设备时钟 vs 进程单调钟(`Instant`) | -3.7 ppm |
| E2 传感器:内置麦 `actual_rate_ppm`(8/15 那场) | -3.11 ppm |
| E2 传感器:内置麦 `inter_track.rel_ppm` | -3.10 ppm |

四者落在 0.6ppm 内,**与"补偿生效"的预期一致**:如果补偿完全没生效,tap 内容会以
"播放侧时钟 − master 时钟"的速率跑偏,而两块独立晶振通常差几十 ppm——这样一项会以
几十 ppm 的斜率直接暴露在斜坡上,实测完全没有。

**但到此为止,不能再往前推。** 观测量 = 补偿残余 + master 时钟误差 + **播放侧时钟误差**,
而上表里没有任何一路独立量到最后一项(Codex 二轮 P2 指出):它们量的要么是这条斜率
本身,要么是 mic/聚合设备的时钟。所以:

- 减去 master 估计**不能**分离出补偿残余,也就给不出"残余 ≲1ppm"这种数字;
- 还有一个说不清的可能:这台机器的蓝牙耳机与内置麦的晶振本就相差 ~1ppm,那么即使
  补偿一点没生效,读数也长这样。

要把话说死有两条路,都还没走:**①声学口径**——mic 真听见播放内容,共模项按构造抵消,
量到的就是残余本身;**②独立量播放侧时钟误差**,再从这条斜率里减掉。

错位曲线的线性度同样是判据 2 的证据:6 分钟内最大偏离直线 0.042ms,没有任何台阶——
系统若做了内部硬校正,这里会看到跳变。

复现命令(分析脚本已入库):

```bash
python3 scripts/drift_vs_ref.py <参考.wav> <采集目录>/system.wav
```

**为什么不是 `bin/xcorr_align`**:它只收 corr>0.5 的窗,而这次大量窗落在 0.1~0.2,
按它跑只剩 6 个点(斜率 -3.36ppm,与全窗结果差 0.1ppm)。低相关窗的**峰位置仍然正确**
——错位读数一个不落地排在同一条斜坡上,被压低的只是峰值。原因是探针那个无抗混叠的
48k→16k 抽取:相位随漂移缓慢滑移,经过整采样点时重合度高、离开时掉下去。
所以分析脚本单独入库、默认用全部窗并打印每个窗的 corr 供核对。要根治相位滑移就加
`--native` 按原始率落盘(轨间比对不受影响,那条路两轨走同一条抽取)。

## 判据逐条对照(预注册,`clock-drift-sensor-design.md` 第五节)

| # | 判据 | 状态 |
| --- | --- | --- |
| 1 | 残余轨间漂移中位 <20ppm 且 1h 错位 <10ms | **未收口**:参考对照 -3.26ppm 与"补偿生效"一致,但分离不出残余(播放侧时钟未独立测);判据口径(声学)待做 |
| 2 | 错位曲线无跳变(无系统内部硬校正) | **通过**:370s×2 场时间戳断层 0 次;错位曲线最大偏离直线 0.042ms |
| 3 | 设备热插拔不崩、行为可解释 | **未测** |
| 4 | TCC 授权体验可接受 | **部分**:从终端跑无提示、无拒绝,tap 直接建成并拿到真实系统音频;签名 .app 内的表现未测 |

判据 1 为什么说"未正式收口":本次量的是参考对照口径,里面混着共模项,反推残余要靠
减法与一个 ±0.6ppm 量级的不确定度(见上节)。判据原文说的轨间错位要用**声学口径**
(mic 真听见播放内容)直接量——那条口径里共模项按构造抵消,读数就是残余本身。
预注册的判据不能拿"按推理应该没问题"顶替实测。

判据 4 需要补充说明:本次是从终端(继承终端的 TCC 身份)跑的,**没有弹任何授权框**,
tap 与聚合设备都建成了、也真的拿到了系统音频内容。这不等于 app 内也一样——
产品化要在签名 bundle 里复测,并确认 Info.plist 需要哪些用途声明。

## 还差什么才能裁决

1. **声学口径的判据 1**:内置麦 + **内置扬声器**、安静房间、≥10 分钟,播 E1 标定序列,
   `xcorr_align system.wav mic.wav` 取斜率。10 分钟能分辨到 ~2ppm(1 样本 = 62.5µs)。
2. **热插拔(判据 3)**:采集中途拔/换 mic 设备、切默认输出设备,观察 IOProc 是否继续、
   聚合设备是否失效、进程是否崩,以及断层计数。
3. **签名 app 内的 TCC(判据 4)**:同一套代码放进 bundle 跑一遍。
4. **13.x–14.1 双栈成本**:app 最低 macOS 13.0,CATap 要 14.2+。走 A 就得同时维护
   新旧两条采集路径,这笔账要与 B(DLL 闭环 + rubato 变比重采样)的实现成本一起算。

## E2 现状(同一 issue 的另一半)

`bin/drift_stats` 加了 `--since`,把 PR#103 之前的污染场次挡在基线之外
(那批重锚是"用 tap 线程到达间隔测采样率"这个错误测量骗出来的,详见工具文件头)。
本机当前:

```
$ cargo run --bin drift_stats -- <data_root> --since 20260814-170000
按 --since 挡掉的报告: 3 份
场次: 2  含降级源: 0
含 inter_track 的场次: 2/2
|轨间漂移| ppm  P50=3.098 P95=3.098 max=3.098
重锚总数: 0
按设备: (未知设备) 1 场 0 重锚 / MacBook Pro麦克风 1 场 0 重锚
```

两场:内置麦组合 −3.10ppm(−11.2ms/h)、蓝牙 16k HFP 组合 0.0006ppm(BT 栈按 host 重定时)。
**n=2,远不够裁决**,覆盖目标(内置/AirPods/有线/USB 各若干场)得靠日常使用累积。
