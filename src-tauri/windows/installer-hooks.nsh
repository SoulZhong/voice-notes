; Windows 安装/卸载钩子(2026-08-22,用户实测反馈修复)。
;
; 病因:v0.12.0 安装时报 "Error opening file for writing: ...voice-notes.exe"。
; 目标 exe 被占用——两类进程会锁它:①主程序托盘常驻(用户点关窗以为退出了,
; 托盘里还活着);②MCP 后台(`voice-notes.exe mcp serve`,由 MCP 客户端拉起,
; 用户完全无感)。默认 NSIS 模板的"应用在运行"检查覆盖不到这两类。
;
; 处置:先不带 /F 礼貌关闭(给主程序一次正常退出的机会),稍候再 /F 强制清尾
; (托盘进程通常忽略 WM_CLOSE,MCP 后台没有窗口)。taskkill 按映像名匹配,
; 一次清掉全部实例。查无进程时 taskkill 返回非零,忽略即可。
!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToLog 'taskkill /IM "voice-notes.exe"'
  Sleep 800
  nsExec::ExecToLog 'taskkill /F /IM "voice-notes.exe"'
  Sleep 400
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog 'taskkill /F /IM "voice-notes.exe"'
  Sleep 400
!macroend
