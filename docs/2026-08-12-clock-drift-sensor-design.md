# 双轨时钟漂移传感器 + A/B 裁决实验设计

日期:2026-08-12 · 依据:`2026-08-12-clock-sync-industry-solutions.md`(业界方案调研)· 用户已批准

## 目标

把双轨(mic + 系统声)时钟漂移从"测不出"(现状:到达墙钟对账,容差 1% = 10000ppm,晶振级 50–500ppm 漂移全盲)变成**每台机器、每场录音的连续在线量**;并以此为裁判,用真实数据在两条修正路线间裁决:

- **路线 A**:macOS 14.2+ 私有聚合设备(mic 主时钟 + CATap),drift correction 由系统闭环;
- **路线 B**:自持 DLL + 变比重采样(全版本可用,ppm 级精度,全程可观测)。

分期铁律:**一期只测不动数据**——不改任何音频样本、不动现有 1% 对账逻辑(两者并行输出、互为校验)。传感器是无悔投入:它是 B 的必要组件,也是 A(黑盒)唯一的验收仪器。

设计取舍背景:漂移数值随设备对、温度、时段变化,开环标定必死,必须闭环在线估计——异构硬件不是本方案的障碍,而是它存在的理由。

## 一、DLL 核心(新模块 audio/drift_dll.rs)

Adriaensen 二阶 DLL(LAC 2005/2012),纯函数、无 I/O、无系统调用,可独立单测。

- **状态更新**(每测量周期):
  ```
  e  = t_观测 − t_预测        # 相位误差
  t1 += b·e + e2              # 相位修正 + 当前周期估计
  e2 += c·e                   # 频率状态(ppm 漂移住在这里)
  ```
  系数 `b = √2·ω, c = ω², ω = 2π·B/F`(F = 测量频率)。
- **参数**(常量,标注"调研共识初值,标定实验后校准"):
  - 稳态带宽 `DLL_BW_STEADY = 0.05 Hz`(zita 0.05 / PipeWire 0.016–0.128 共识区);
  - 启动加速:首点硬对齐,随后 `DLL_WARMUP_SECS = 4.0` 内用 `DLL_BW_WARMUP = 0.5 Hz`,再降稳态(zita 配方,总收敛 ~15s);
  - 重锚阈值 `DLL_REANCHOR_ERR_MS = 5.0`:相位误差超此值 → 内部自动重锚保频率(soft:只清相位,不清频率状态,`push()` 内部处理),不让环路慢慢爬回;设备切换/采样率变化走外部显式重锚,语义与阈值无关(见第三节)。
- **输入**:单调递增的(累计样本数,host 时刻 ns)测量点。
- **输出**:`DriftEstimate { rate_ppm, phase_err_us, converged, reanchor_count }`——`rate_ppm` 即频率状态换算的"实测速率相对标称的偏差";另提供 `map_sample_to_host_ns()` 平滑映射(二期执行端/验收用)。

## 二、硬件时间戳接入(audio/ 三条采集后端)

`AudioFrame` 增加 `host_time_ns: Option<u64>`(统一换算到纳秒;mach ticks→ns 的 timebase 换算封装一处)。各后端填法:

| 后端 | 时间戳来源 | 备注 |
|---|---|---|
| mic(cpal) | `InputCallbackInfo::timestamp().capture` | **实现期第一件事**:验证 cpal macOS 后端填的是否为 CoreAudio `inInputTime`(硬件口径)。若是软时刻,则按 vpio.rs 的直调先例绕过 cpal 补采时间戳 |
| mic(VPIO) | 输入回调 `AudioTimeStamp.mHostTime` | vpio.rs 已直调 C API,顺手可取 |
| system(SCK) | CMSampleBuffer PTS(mach 时基) | screencapturekit crate 的 sample buffer 携带 |
| Windows | 一期 stub(仓库已有 aec_stub 先例) | WASAPI QPC 时间戳留接口不实现 |

降级语义:某源拿不到硬件时间戳 → `host_time_ns = None`,该源 DLL 改喂到达墙钟,报告标 `quality: "degraded"`——行为等同现状,**绝不假装精确**。

交叉验证:mic 侧同时轮询 CoreAudio `mRateScalar`(系统现成的实测/标称速率比),写进报告作旁证;SCK 侧无此物,不强求。

> **实现期修订**:旁证口径改为 `kAudioDevicePropertyActualSampleRate`(默认输入设备实测采样率),而非逐回调的 `mRateScalar`——两者是等价的设备级速率旁证,但 `ActualSampleRate` 有现成绑定、无需逐帧回调开销;实现为独立线程每 10s 轮询一次(`audio/actual_rate.rs` + `lib.rs` 中的 10s 定时循环),换算成 ppm 写入 `DriftSourceReport.actual_rate_ppm` 字段(报告字段名同步由"rate_scalar_ppm"改为 `actual_rate_ppm`,见下方第四节修订)。

## 三、传感器接线(pipeline/frame_tap.rs)

每源一个 DLL 实例,挂在 frame_tap(逐源健康统计与墙钟对账已在此,归属顺理成章):

- 每帧(或聚每 N 帧成 ~100ms 测量周期)喂 `(该源累计样本数, host_time_ns)`;
- 与现有 1% 容差对账**并行**,互不干扰;两者对同一源给出的速率估计差值本身就是诊断量;
- 补零帧不喂 DLL(它们没有真实时间戳);补零段结束视同重锚事件;
- 暂停:DLL 状态冻结,恢复后硬重锚(时间轴冻结语义与现状一致)。

> **实现期修订(暂停语义与设计草案不符)**:上面"暂停:DLL 状态冻结,恢复后硬重锚"是设计阶段的意图,与实现不符——暂停闸落在 `pipeline/segment_worker.rs`(`paused` 置位期间丢帧,冻结的是**笔记自己的时间轴**),`frame_tap`/`DriftMonitor` 在 segment_worker 的上游,对暂停完全无感:暂停期间上游仍在到达的真实帧照常喂 DLL,不冻结、不重锚。这是刻意保留而非遗漏:暂停期间时间戳本身连续,DLL 没有相位跳变可言,若强行重锚反而会丢掉本可继续估计的频率状态,精度更差。
>
> 后果:`rate_ppm_series`/`events` 的 `t_s` 是**含暂停时长的 host 实时轴**,与笔记自己的时间轴(暂停冻结、不含暂停时长)**不对齐**——二期做时间对齐(E1/E2 等实验、或把 drift 事件映射回笔记时间轴)时须先扣掉该时刻之前累计的暂停时长,否则会有系统性偏差。
>
> 另:device_switch 与 gap_end 若落在同一恢复帧(拔插耳机=断流+换率,物理上一次事件)只计 **一次** device_switch,不叠加 gap_end——full 语义(全清频率+相位)已覆盖 soft 语义(只清相位)的效果,重复计两次会把 `build_report` 的"单场重锚 >3 次"异常阈值撞出误报(2026-08-12 终审 P1 修复;回归锁见 `frame_tap.rs` 测试 `device_switch_after_gap_counts_once_not_twice`)。
>
> **实现期修订(重锚语义:full/soft 二分)**:重锚不再是单一"重新初始化"动作,区分 `full`(dll 全清,含频率状态)与 `soft`(保留频率状态,只清相位)两种,由 `mark_reanchor(why, full)` 的 `full` 参数选择:
> - **rate_fix → soft**(设计草案原意是 full/清空):既有 1% 容差对账改写下游采样率(`health.rate_fixes` 计数)时触发。用户已裁定:rate_fix 恰恰是对账**证实**了 DLL 正在测的偏差(同一颗晶振、同一声明率下频率状态仍然有效)——不是"判断失灵要推倒重来",清空频率状态反而会把最该出数的设备的 ppm 抹掉;只清相位是因为改写声明率会给下游引入一次相位跳变,这笔相位差不能沿用旧锚点。
> - **device_switch → full**:设备真的换了(采样率或声道数变化),晶振不再是同一颗,频率状态对新设备没有意义,连同相位一并全清。
> - **gap_end(补零段结束)→ soft**,且**接线顺序是先 `mark_reanchor` 后 `feed`**(设计草案未预见此顺序要求):补零段普遍 ≥ `fill_after`(500ms+),远超 DLL 内部自动重锚阈值(`DLL_REANCHOR_ERR_MS = 5.0`)。若先喂恢复帧再重锚,`push` 会先触发一次 DLL 内部自动重锚(`reanchors` 内部 +1)并把巨大相位误差写入相位状态,随后显式 `mark_reanchor` 又计一次——同一次断流被计两次重锚,足以把 `build_report` 的"单场重锚 >3 次"异常阈值撞出误报。先重锚(`soft`,内部先置"未锚定"状态)再喂帧,恢复帧的 push 走"首点纯锚定"早退分支,不触发内部自动重锚,每个 gap 恰好计 1 次。

轨间相对量:两源 DLL 都在 host 时基上,相对漂移 = 两条速率估计之差;累计错位 = 两条映射在同一 host 时刻的样本位置差。

> **新增(设计文档未预见,实现期增补)**:
> - **DriftMonitor 声明率自动重锁**(`nominal_relock`):frame_tap 转发的帧 `sample_rate` 中途变化(拔插耳机/切设备)时,`DriftMonitor::feed` 内部重建 DLL 实例并重锁 `nominal_hz`;旧实例的重锚计数结转到 `reanchor_carry`(否则 `snapshot()` 直读新实例会把切换前的重锚历史静默清零),`converge_secs` 清空重新计时。首帧惰性锁定(`DriftMonitor::new(0)` 的"标称率未知,以首帧声明率为准"哨兵路径)不算真正换率,不记事件、不结转计数。
> - **frame_tap 设备格式变化分支**:`frame_tap.rs` 中原有的"声明格式变化 → 丢弃旧实测结论、按新声明值从头核对"分支,顺带调用 `mark_reanchor("device_switch", true)` 触发全清重锚,与上方 DriftMonitor 侧的 `nominal_relock` 是同一物理事件的两处响应(一处管 DLL 状态,一处管报告事件与计数口径)。

## 四、数据出口(全本地,无网络上报)

1. **每场落盘** `note_dir/drift_report.json`:
   ```json
   {
     "schema": 1,
     "sources": { "mic": { "nominal_hz": 48000, "quality": "hw|degraded",
       "rate_ppm_series": [[t_s, ppm], ...],   // 每 10s 一点
       "converge_secs": 14.2, "reanchors": 1, "actual_rate_ppm": 23.5 },
       "system": { ... } },
     "inter_track": { "rel_ppm": 87.0, "est_misalign_ms_per_hour": 313.2 },
     "events": [ { "t_s": 601.2, "kind": "reanchor_full", "why": "device_switch" } ]
   }
   ```
   > **实现期修订**:
   > - `events[].kind` 实际取值为 `reanchor_full`(设备切换等全清场景)/`reanchor_soft`(rate_fix、补零段结束等保频率场景)/`nominal_relock`(声明率变化触发的自动重锁;首帧惰性锁定不记事件)——而非草案示例里字面的 `"reanchor"`,三种 kind 的语义与触发点见上方第三节修订。
   > - `events[]` 每条记录挂在其所属源的 `sources.<name>.events` 数组下(实现按源分桶,而非草案示例的顶层单一数组带 `source` 字段;字段集也相应去掉 `source`,只留 `t_s`/`kind`/`why`)。
   > - `rate_scalar_ppm` 字段名改为 `actual_rate_ppm`(旁证口径变更,见上方第二节修订)。
   > - `inter_track` 字段名为 `rel_ppm`(非 `rel_ppm_median`——单值差,非序列统计量)。
2. **异常打点**:|漂移|>500ppm、时间戳缺失、单场重锚 >3 次 → 记入 drift_report 的 `anomalies` 数组 + eprintln 日志(勘误:原写"ailog 切面",但 ailog 是 AI 调用日志、telemetry 是网络遥测,均不合适;全本地红线下就地落报告)。
3. **汇总工具** `bin/drift_stats.rs`:扫全部笔记的 drift_report,输出分布(P50/P95/最差设备组合/degraded 占比)——"真实分布"的出口,也是二期裁决的输入。

   > **实现期修订(设备组合维度降到二期)**:当前 `DriftSourceReport`/`drift_report.json` schema 不带任何设备标识(型号/名称/唯一 ID)——一期采样点只知道"哪个源(mic/system)"和"标称率是多少",不知道"哪台设备",无法按设备组合分桶,"最差设备组合"因此在一期不可实现,不是漏做而是当前 schema 结构性做不到。降到二期:届时 schema 加 device 字段(设备名/型号,采集端已知但未透传到 drift 报告),`drift_stats` 才能按设备组合聚合"最差组合"。
   >
   > 一期 `bin/drift_stats.rs` 的实际输出(见其源码与内测 `ingest_and_percentiles`):场次数、含 degraded 源的场次占比、轨间 `|rel_ppm|` 的 P50/P95/max、全库重锚总数、异常清单(逐条打印,截断前 20 条)。

旁路契约(同 mixed 轨先例):传感器任何失败(时间戳异常、DLL 数值异常、写盘失败)只丢 drift_report,不影响录音主链路。

## 五、二期:A/B 裁决实验(预注册)

| 实验 | 内容 | 回答 |
|---|---|---|
| E1 标定 | 播放已知 click/chirp 序列双轨同录,互相关量真实错位曲线,对照传感器估计 | 传感器自身精度(目标:亚毫秒),使其具备裁判资格 |
| E2 基线 | 日常真实会议若干场 + 开发机设备组合(内置 mic/AirPods/USB)标准场景 | 真实硬件上漂移多大、多稳定 |
| E3 A-spike | 14.2+ 私有聚合设备(`subdevices:[mic(master)], taps:[CATap], private:1, drift:1`),同场景采集,用 E1 方法量残余错位/延迟/CPU,考察热插拔与 TCC 授权 | 路线 A 的真实成色 |

> **实现期修订(E1 工具落地,`bin/xcorr_align.rs` + `scripts/drift-calibration.md`)**:互相关工具调用顺序固定为 `xcorr_align <note_dir>/system.wav <note_dir>/mic.wav`(system 在前、mic 在后)——这样输出的斜率符号与 `drift_report.json` 的 `inter_track.rel_ppm`(定义为 `mic.rate_ppm - system.rate_ppm`)直接同号可比,调换顺序会导致符号相反。判据口径不变:两者斜率之差 < 5ppm 视为传感器精度达标,可作裁判。详细步骤见 `scripts/drift-calibration.md`。

**裁决标准(现在预注册,防事后拍脑袋)**——A 达标 = 同时满足:

1. 残余轨间漂移中位 < 20ppm 且错位有界不累积(1 小时 < 10ms);
2. 错位曲线无跳变(无系统内部硬校正的咔哒/跳点);
3. 设备热插拔不崩、行为可解释;
4. TCC 授权体验可接受(相对 SCK 不显著变差)。

**A 达标** → A 为主路径(14.2+),13.x–14.1 保留现状 + 传感器诊断;可选后手:用传感器实测率喂现有相位还款,把 1% 容差收紧到 ppm 级(旧系统的减配 B)。
**A 不达标** → 立项 B 执行端:DLL 闭环 + `rubato` 流式变比重采样(误差预滤波 20×带宽;比率步进 ppm 级限幅;48k→48k 同标称率的分数误差按 zita `inpdist()` 思路修正)。

## 六、错误处理与边界

- 设备热插拔/采样率切换:复用现有 StreamResampler 重建触发点,同步触发 DLL 重锚;
- mach timebase 换算只做一次,封装在 `host_time` 小模块;
- 续录:新段新 DLL(与"续录快照不进票池"同哲学,跨段不复用陈旧状态);
- 溢出/时钟回绕:host_time ns 用 u64,单调性校验,非单调点丢弃并计数。

## 测试

- **DLL 单测**:合成注入已知 ppm 漂移(+固定/±随机抖动、时钟量化),断言:收敛后 `rate_ppm` 误差 < 1ppm、收敛时间 < 20s、重锚后再收敛;零漂移输入不产生虚假漂移。
- **frame_tap 集成**:模拟带时间戳帧流(含补零段、暂停、设备切换),断言报告数值与事件记录。
- **schema 测试**:drift_report 序列化往返 + degraded 降级路径。
- **E1 标定脚本入库**(`scripts/` 或 `bin/`),作为可重复验收工具,二期直接复用。
