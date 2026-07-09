# 回放增益归一化(A1)— 设计

日期:2026-07-09
状态:待实现
关联:backlog A1(「回放无声/太轻」定案,2026-07-07);播放器 `src/lib/AudioPlayer.svelte`

## 背景与目标

会议软件会把系统输入音量拉低,`keep_output_volume`(普通麦克风)模式下无增益补偿,导致早期笔记录出的波形近乎无声。这些老笔记的波形已固化在磁盘,重录不可能;录制侧的 AGC 修复只对**之后**的录音有效。本功能在**回放侧**补救:打开老笔记时把整条录音的响度抬到接近正常水平。

**成功标准**:一条输入音量 30 时代录的老笔记,打开后能以接近正常响度听清,且不失真;AGC 之后录的正常笔记回放电平不受影响。

**非目标(YAGNI)**:
- 不做动态压缩/多段限幅——单一静态增益即可。
- 不做「缩小」——只增不减,绝不把正常录音弄轻。
- 不改录制/采集链路,不改磁盘上的波形或音频。
- 不做每轨独立增益(见「决策」)。
- 不改后端,不做数据回填。

## 决策记录(brainstorm 拍板)

1. **激活方式 = 自动 + 可关开关**:默认开(「自动」语义),播放器上给一枚开关可一键听原始电平。
2. **增益粒度 = 整条笔记一个增益**:保留 mic/系统声的自然比例,正对「系统输入音量被拉低→整条都轻」的真实病因;不会把 mic 路的 AEC 残渣/噪声轨单独放大。
3. **引入 vitest**:为增益纯函数兜底(此前项目无 JS 测试框架)。

## 架构

纯前端,零后端改动。老笔记本就有 `TrackInfo.waveform`(缺失的走既有懒回填);`HTMLMediaElement.volume` 封顶 1.0 无法增益,故用 WebAudio 一个共享 `GainNode` 提升整条笔记响度。

信号链(全部在 `AudioPlayer.svelte` 内):

```
每轨 <audio> ──MediaElementSource──┐
每轨 <audio> ──MediaElementSource──┤→ 共享 GainNode(noteGain) → AudioContext.destination
        …                          ┘   (整条笔记一个增益)
```

- 一个 `AudioContext`(懒建;首次 `play()` 用户手势里 `resume()`,绕过自动播放策略);组件销毁 `close()` 防泄漏。
- 静音沿用现有 `el.muted`;**实现时须验证经 WebAudio 路由后 `el.muted` 仍生效**,若 WebKit 下失效则退化为每轨一个 `muteGain` 节点(0/1)串在 source 与共享 gain 之间。
- 关闭归一化 = `gainNode.gain` 目标 1.0;打开 = 计算值。切换一律 `setTargetAtTime`(短时间常数,~20ms)平滑,避免咔哒。

### 单元:增益计算(纯函数)

抽成独立可测模块 `src/lib/gain.ts`:

```
computeNoteGain(tracks: TrackInfo[]): number
```

输入各轨 `waveform`(0..255 绝对峰值桶,`v = |i16sample| >> 7`,255=满幅),输出应用到共享 `GainNode` 的增益(≥1)。

算法:
1. 汇总所有轨的非零桶值。任一有效桶都没有 → 返回 `1`(无从判断,不猜)。
2. **响度代理** `L` = 所有非零桶的 ~90 百分位值(避开单个瞬态尖峰),0..255。
3. **绝对峰值** `P` = 所有桶的最大值,0..255。
4. `gain = min(TARGET / L, CEILING / P, MAX_BOOST)`;再 `gain = max(1.0, gain)`(只增不减)。
5. `L == 0`(全静音)→ 返回 `1`(守卫除零)。

常量(初拟,冒烟再调,集中在模块顶部):
- `TARGET = 170`(目标响度代理,≈良好录音的常态电平)
- `CEILING = 250`(放大后峰值上限,留余量不削顶)
- `MAX_BOOST = 8`(最大放大倍数,防把噪声地板轰起来)

不变量:`CEILING / P` 保证 `P * gain ≤ CEILING < 255`——**构造上不削波**,无需额外限幅器。

## UI 与持久化

- 播放器控制区(现有静音胶囊那一行)加一枚 **「响度」开关**(pill/toggle,复用静音胶囊同族样式与 DESIGN.md 现有 token,不引新组件)。
- 仅当本条 `computeNoteGain(tracks) > 1` 时显示该开关;钳到 1.0(好录音/无波形)时**隐藏**——不给「开了没反应」的死开关。
- 状态存 `localStorage` 键 `vn.playbackNormalize`,**未设时默认 `true`**(实现「自动」);用户关过一次即记住,跨笔记与重启保持。
- 开关状态驱动共享 `GainNode`:on → `computeNoteGain` 值;off → 1.0。

## 边界与错误处理

1. 某轨无 `waveform` 或全笔记无有效桶 → `computeNoteGain` 返回 1,开关隐藏。
2. 全静音笔记(所有桶=0)→ `L=0` 守卫,gain=1。
3. 已够响(`P` 近满幅)→ `CEILING/P` 钳到 1.0,无变化,开关隐藏。
4. **轨道列表变化**(续录 / `transcode_done` 事件会重拉音轨、重建 `els`):每个 `<audio>` 只能建一次 `MediaElementAudioSourceNode`,故按元素缓存 source 节点,随 `els` 变化增量重连;`GainNode` 复用。tracks 变化后重算 `computeNoteGain`。
5. `AudioContext` 可能起于 `suspended`:`play()`(用户手势)里 `resume()`;组件卸载 `close()`。
6. 静音 × 归一化并存:验证 `el.muted` 经路由仍生效(见架构)。
7. 归一化与既有拖拽定位/多轨同步/后台播放逻辑正交——只在 destination 前加一级增益,不碰时钟与 `currentTime` 同步。

## 测试与验证

**引入 vitest**(本功能一并立起 JS 单测基座):
- 加 `vitest` 到 devDependencies、`vitest.config.ts`、`package.json` 脚本 `"test": "vitest run"`(与既有 `check` 并列)。
- `src/lib/gain.test.ts` 覆盖 `computeNoteGain`:
  - 低电平波形 → gain 显著 >1,且 `P * gain ≤ CEILING`(不削波不变量)。
  - 满幅波形 → gain === 1(钳制生效)。
  - 全静音(桶全 0)→ gain === 1(除零守卫)。
  - 无 `waveform` 的轨 / 空 tracks → gain === 1。
  - `MAX_BOOST` 上限:极轻波形 gain 不超过 8。
  - 多轨:整条按合并统计算一个增益(而非逐轨)。

**浏览器冒烟**(既有套路:Playwright 打 `localhost:1420` + `__TAURI_INTERNALS__` shim 注入假 tracks):`evaluate` 读共享 `gainNode.gain.value` 断言随开关切换在计算值与 1.0 之间变化。

**真机冒烟**:
- 开一条已知很轻的老笔记(输入音量 30 时代)→ 开关开确认明显变响、不失真;关→回原始电平。
- 开一条 AGC 之后的正常笔记 → 确认开关隐藏 / gain≈1,回放电平不变。
- 切换开关无咔哒;续录一条后停录(音轨重建)→ 归一化仍正确。

## 影响面

- 改:`src/lib/AudioPlayer.svelte`(WebAudio 图 + 开关 UI + localStorage)。
- 新增:`src/lib/gain.ts`、`src/lib/gain.test.ts`、`vitest.config.ts`;`package.json`(vitest 依赖 + test 脚本)。
- DESIGN.md:若开关样式需登记则补一行(复用现有胶囊则可不补)。
- 后端:无。
