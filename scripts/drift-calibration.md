# E1 漂移传感器标定(可重复)

1. 准备刺激音:任何含丰富瞬态的音频(如节拍器/click track),用系统播放器循环播放。
2. voice-notes 正常开录 ≥10 分钟(双轨:mic 收房间回放,system 收播放流)。
3. 停录,找到笔记目录:笔记详情页头部的"打开目录"按钮(notes/<id>/)。
4. 互相关真值(调用顺序为 system.wav 在前、mic.wav 在后 —— 这样输出的符号才直接对齐
   inter_track.rel_ppm = mic.rate_ppm - system.rate_ppm,顺序调换会导致符号相反):
   cargo run --bin xcorr_align -- <note_dir>/system.wav <note_dir>/mic.wav
5. 传感器读数:<note_dir>/drift_report.json 的 inter_track.rel_ppm。
6. 判定:两者之差 < 5ppm 且 offset 曲线形状一致 → 传感器可当裁判(E2/E3 复用本流程)。
   注意:互相关量的是"含起点偏移的错位曲线",传感器量的是斜率;只对比斜率(ppm)。
   口径已通过第 4 步的调用顺序对齐(xcorr_align 输出 ≈ rel_ppm),两者应同号,不需要再取反或加绝对值。
