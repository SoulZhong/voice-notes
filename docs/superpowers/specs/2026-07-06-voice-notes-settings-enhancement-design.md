# 设置增强 + 系统集成 设计

日期:2026-07-06。单阶段单 PR(用户拍板"一期全做")。动因:设置页现状(存储/模型/ASR)太弱。

## 已确认决策

- 四块全做:外观主题切换、录制选项、音频与磁盘、系统集成;**单 PR**。
- 压缩码率不做档位(固定 32k,YAGNI)。
- 音频清理按时间阈值(30/90 天/全部,三选+两段确认)。
- 全局快捷键**可配置**(录入框捕获组合键),默认关。
- 菜单栏常驻 = 托盘图标+菜单,开启时**关窗只隐藏不退出**;默认开。

## settings.json 新增字段(全部 serde default,旧文件兼容)

| 字段 | 类型/默认 | 语义 |
|---|---|---|
| theme | String "system" | "light"/"dark"/"system" |
| record_system_only | bool false | 仅系统声音录制(下一场生效) |
| language_filter | bool true | 中英白名单语言幻觉过滤 |
| keep_audio | bool true | 录音音频旁路落盘 |
| shortcut_enabled | bool false | 全局快捷键开关 |
| shortcut | String "Alt+CmdOrCtrl+R" | 快捷键(tauri accelerator 格式) |
| tray_enabled | bool true | 菜单栏常驻+关窗隐藏 |
| autostart 不进 settings.json | — | 真值源是系统 LaunchAgent,经插件 isEnabled 读取,避免双真值源漂移 |

## 一、外观:主题切换

- 设置页「外观」区块:亮色/暗色/跟随系统 三选(radio,与 ASR 选型同形态)。
- **技术方案:CSS `light-dark()` 重构 app.css**——每 token 单点定义 `--x: light-dark(亮值, 暗值)`,`:root { color-scheme: light dark }`;切换 = 前端设 `documentElement.style.colorScheme`("system" = 清空该内联样式)。启动时 +layout 读 settings 应用,切换即时生效。
- 收益:token 从"亮块+暗媒体查询块"两份收敛为一份,消除双处同步;DESIGN.md 色表(亮/暗两列)不变,仅"落地方式"注记更新。
- 风险与回退:WKWebView 需 Safari 17.5+(本机 macOS 15+,满足);**冒烟第 1 项即验证**,若发现不支持,回退方案 = `:root[data-theme]` 属性 + 复制 token 块(不改 settings 契约)。

## 二、录制选项

- **仅系统声音**(record_system_only):开启时 spawn_session 只构建 System 源;「mic 必备」守卫(Fix A)泛化为「按配置的必备源集合」——system_only 时 System 启动失败才 error,mic 相关降级横幅逻辑不触发。VPIO/AEC 不启动(无 mic 路);回声去重/残渣抑制单路自动无操作。录制中改设置不影响本场(与 ASR 选型同语义)。
- **语言幻觉过滤**(language_filter):`session.rs::is_foreign_final` 的调用受开关控制;开关经 `start_session` 新参数传入(不走全局静态量)。关闭后日/韩段照常入笔记。
- **保留录音音频**(keep_audio):关闭时 spawn_session 不建 AudioTrackWriter sinks 与写盘线程;转写、声纹、时长语义零影响;已有笔记音频不动;转码队列对无 wav 的笔记天然无事。

## 三、磁盘

- 统计:新命令 `audio_disk_usage() -> u64`——walk notes 根目录累计 `*.m4a`+`*.wav` 字节(含 `.m4a.bad`;声纹样本目录不计——它属声纹库且极小)。设置页「磁盘」区块显示「录音音频占用 X」。
- 清理:新命令 `purge_audio(older_than_days: Option<u32>)`(None=全部):遍历 `state=complete` 且非活动、非转码中的笔记(先 `transcode.pause_and_wait()`,清理完 unpause),`ended_at`(缺失回退 started_at)早于阈值 → 删该笔记全部音频轨文件(m4a/wav/.bad)并清 audio.json 各 track 的 codec/duration(offset 保留,无害);笔记文字/speakers/声纹样本全保留。返回释放字节数,前端刷新统计。
- UI:N 选 30 天前/90 天前/全部,两段确认(danger 语义,文案说明"只删音频,文字与说话人保留");录制中禁用。

## 四、系统集成

### 全局快捷键(tauri-plugin-global-shortcut)

- 开关 + 录入框:录入框聚焦后捕获下一个组合键(keydown 组装 accelerator 字符串,显示为 mac 符号如 ⌥⌘R),Esc 取消,存 settings 并即时重注册。
- 触发语义 = **切换录制**:空闲(且 recording_ready)→ 开录;录制中/暂停 → 停止。走既有 start/stop 命令同一实现(spawn_session/stop 逻辑复用,不复制)。
- 注册失败(冲突/系统拒绝)→ 设置页红字提示,开关自动回落关;开关关闭/应用退出注销快捷键。启动时按 settings 注册。

### 开机自启(tauri-plugin-autostart)

- LaunchAgent 方式;设置页开关直连插件 enable/disable/isEnabled(读系统真值,不存 settings.json)。

### 菜单栏常驻(Tauri v2 内建 TrayIcon)

- 模板线框图标(黑白,随系统菜单栏自适应);**录制中切红点变体**(两套 icon 资源)。
- 菜单:「开始录制/停止录制」(按状态动态文案,recording_ready 为假时禁用)、「打开主窗口」(show+focus)、「退出」(真退出,录制中先走 stop_recording 收尾再退)。
- `tray_enabled=true` 时主窗 CloseRequested 拦截为 hide(录制不中断);false 时恢复默认关窗即退,且不建托盘。开关变更即时生效(建/销托盘+改关窗行为)。
- 托盘状态同步:lib.rs 录制状态变化点(start 成功入槽/stop/error 清 running)调用 tray 更新函数(图标+菜单文案);不依赖前端事件转发。

## 五、设置页结构

新增四区块沿现有单页滚动卡片形态,顺序:外观 → 录制 → 磁盘 → 系统(集成)→ 既有(存储/模型/语音识别)保持不动。全部开关即改即存(setSettings);录制中的禁用面:仅系统声音/语言过滤/保留音频三项允许改(下一场生效,与 ASR 一致的"录制中禁改"?——否:这三项不重建常驻模型,**录制中允许改**,注明下一场生效;快捷键/托盘/主题随时可改)。

## 错误处理总则

全部增值层姿态:快捷键注册失败回落、托盘创建失败降级打日志(应用照常)、清理失败逐笔记 continue 并回报已释放量、autostart 插件错误红字提示。绝不影响录制与转写。

## 测试

- settings 新字段 roundtrip + 旧文件兼容(既有测试模式扩展)。
- 必备源集合泛化:system_only 下 System 失败 → error,mic 不参与判定(session 层单测,mock capture)。
- 语言过滤开关:关闭时日语标签段不被丢弃(worker 级既有测试参数化)。
- keep_audio=false:无 audio sinks(spawn 层难单测 → 冒烟)。
- purge_audio:时间阈值筛选、活动/转码笔记跳过、只删音频文件、audio.json 清标记、返回字节数(tempdir 造笔记全可测)。
- audio_disk_usage:统计口径(m4a+wav+.bad,不含声纹样本)。
- 快捷键 accelerator 组装纯函数(keydown 事件 → "Alt+CmdOrCtrl+R" 字符串)单测(前端无框架 → 逻辑放 ts 纯函数,冒烟验证;或 Rust 侧不涉及)。
- 托盘/自启/全局快捷键系统行为:人工冒烟。

## 验收冒烟

1. 主题三档切换即时生效、重启保持;light-dark() 在 WKWebView 正常(否则触发回退方案);
2. 开「仅系统声音」外放录一场:无 mic 垃圾段、无日语幻觉残渣;关闭恢复双源;
3. 关语言过滤录一场含日语音频:日语段出现在笔记;
4. 关「保留音频」录一场:无音频轨,播放器不出现,转写正常;
5. 磁盘统计显示合理值;清理「全部」后统计归零、笔记文字完好、活动笔记音频保留;
6. 快捷键录入 ⌥⌘R,应用后台按下可开录/停止;冲突键(如 ⌘C)注册失败有提示;
7. 关窗后录制继续、托盘图标变红点、托盘可停止/回窗/退出;关闭常驻开关后关窗即退;
8. 开机自启开关后在系统「登录项」可见,重启登录自动拉起。

## 已知取舍

- autostart 状态不进 settings.json(系统 LaunchAgent 是唯一真值源,双写必漂移)。
- 快捷键录入只做单组合键捕获,不做多快捷键/宏;冲突检测依赖注册失败反馈,不做预检。
- 托盘「退出」在录制中先收尾再退,可能有秒级延迟(转写排干),不做强杀。
- 清理跳过"已中断"(recording 态)笔记——它们可续录,音频是活数据。
- light-dark() 若 WKWebView 不支持则回退复制块方案(设计内置,冒烟裁决)。
