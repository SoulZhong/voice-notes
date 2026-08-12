# E1 漂移传感器标定(可重复)

> **前提(Codex review 终审发现)**:默认 capture_path=aec 下,软件 AEC 在 mic.wav
> 落盘前工作,会把本场播放的 click 当成"系统回放→mic 回声"定向消除掉——标定的
> 刺激没了,xcorr 的 0.5 相关阈值会拒掉所有窗,标定测不出任何东西;capture_path=vpio
> 档同理(Apple AEC)。标定场必须以 `VOICE_NOTES_CALIBRATION=1` 启动 app,让本场
> 强制走普通 cpal 麦克风(不进 VPIO)且跳过软件 AEC 角色构建,两处均不进设置系统/
> UI,是校准的唯一例外。该场录音会有回声(mic 能听见系统外放)属预期,不是缺陷。
>
> 从终端启动,任选其一:
> ```sh
> # 已打包 app(macOS):
> VOICE_NOTES_CALIBRATION=1 /Applications/voice-notes.app/Contents/MacOS/voice-notes
> # 开发模式:
> VOICE_NOTES_CALIBRATION=1 cargo tauri dev
> ```
> 启动后终端应能看到一行 `[标定模式] AEC 已停用(本场)`,确认旁路已生效再开始录制。

1. 准备刺激音:任何含丰富瞬态的音频(如节拍器/click track),用系统播放器循环播放。
2. 以上述命令启动 app(VOICE_NOTES_CALIBRATION=1),正常开录 ≥10 分钟(双轨:mic 收
   房间回放,system 收播放流)。
3. 停录,找到笔记目录:笔记详情页头部的"打开目录"按钮(notes/<id>/)。
4. 互相关真值(调用顺序为 system.wav 在前、mic.wav 在后 —— 这样输出的符号才直接对齐
   inter_track.rel_ppm = mic.rate_ppm - system.rate_ppm,顺序调换会导致符号相反):
   cargo run --bin xcorr_align -- <note_dir>/system.wav <note_dir>/mic.wav
5. 传感器读数:<note_dir>/drift_report.json 的 inter_track.rel_ppm。
6. 判定:两者之差 < 5ppm 且 offset 曲线形状一致 → 传感器可当裁判(E2/E3 复用本流程)。
   注意:互相关量的是"含起点偏移的错位曲线",传感器量的是斜率;只对比斜率(ppm)。
   口径已通过第 4 步的调用顺序对齐(xcorr_align 输出 ≈ rel_ppm),两者应同号,不需要再取反或加绝对值。
