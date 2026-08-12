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
  - 重锚阈值 `DLL_REANCHOR_ERR_MS = 5.0`:相位误差超此值(或设备切换/采样率变化/长补零)→ 重新初始化 + 硬重锚,不让环路慢慢爬回。
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

## 三、传感器接线(pipeline/frame_tap.rs)

每源一个 DLL 实例,挂在 frame_tap(逐源健康统计与墙钟对账已在此,归属顺理成章):

- 每帧(或聚每 N 帧成 ~100ms 测量周期)喂 `(该源累计样本数, host_time_ns)`;
- 与现有 1% 容差对账**并行**,互不干扰;两者对同一源给出的速率估计差值本身就是诊断量;
- 补零帧不喂 DLL(它们没有真实时间戳);补零段结束视同重锚事件;
- 暂停:DLL 状态冻结,恢复后硬重锚(时间轴冻结语义与现状一致)。

轨间相对量:两源 DLL 都在 host 时基上,相对漂移 = 两条速率估计之差;累计错位 = 两条映射在同一 host 时刻的样本位置差。

## 四、数据出口(全本地,无网络上报)

1. **每场落盘** `note_dir/drift_report.json`:
   ```json
   {
     "schema": 1,
     "sources": { "mic": { "nominal_hz": 48000, "quality": "hw|degraded",
       "rate_ppm_series": [[t_s, ppm], ...],   // 每 10s 一点
       "converge_secs": 14.2, "reanchors": 1, "rate_scalar_ppm": 23.5 },
       "system": { ... } },
     "inter_track": { "rel_ppm_median": 87.0, "est_misalign_ms_per_hour": 313.0 },
     "events": [ { "t_s": 601.2, "kind": "reanchor", "source": "mic", "why": "device_switch" } ]
   }
   ```
2. **异常打点**:|漂移|>500ppm、时间戳缺失、单场重锚 >3 次 → 记入 drift_report 的 `anomalies` 数组 + eprintln 日志(勘误:原写"ailog 切面",但 ailog 是 AI 调用日志、telemetry 是网络遥测,均不合适;全本地红线下就地落报告)。
3. **汇总工具** `bin/drift_stats.rs`:扫全部笔记的 drift_report,输出分布(P50/P95/最差设备组合/degraded 占比)——"真实分布"的出口,也是二期裁决的输入。

旁路契约(同 mixed 轨先例):传感器任何失败(时间戳异常、DLL 数值异常、写盘失败)只丢 drift_report,不影响录音主链路。

## 五、二期:A/B 裁决实验(预注册)

| 实验 | 内容 | 回答 |
|---|---|---|
| E1 标定 | 播放已知 click/chirp 序列双轨同录,互相关量真实错位曲线,对照传感器估计 | 传感器自身精度(目标:亚毫秒),使其具备裁判资格 |
| E2 基线 | 日常真实会议若干场 + 开发机设备组合(内置 mic/AirPods/USB)标准场景 | 真实硬件上漂移多大、多稳定 |
| E3 A-spike | 14.2+ 私有聚合设备(`subdevices:[mic(master)], taps:[CATap], private:1, drift:1`),同场景采集,用 E1 方法量残余错位/延迟/CPU,考察热插拔与 TCC 授权 | 路线 A 的真实成色 |

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
