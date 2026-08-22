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
; 追加实测(同日):用户任务管理器杀掉后进程"又自动拉起来"——MCP 客户端(配置了
; voice-notes 的 AI 工具)对连接断开做自动重连,毫秒级重新拉起服务进程,taskkill
; 与文件写入之间的窗口必输。绕开这场竞赛靠 Windows 特性:**运行中的 exe 不能被
; 覆盖,但可以被改名**。杀完把旧 exe 改名挪开(改名对运行中的进程同样成功),
; 安装器写入的目标名从此干净;监工下次拉起时拿到的就是新版。旧文件标记重启后删。
!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToLog 'taskkill /IM "voice-notes.exe"'
  Sleep 500
  nsExec::ExecToLog 'taskkill /F /IM "voice-notes.exe"'
  Sleep 300
  Delete "$INSTDIR\voice-notes.exe.old"
  Rename "$INSTDIR\voice-notes.exe" "$INSTDIR\voice-notes.exe.old"
  Delete /REBOOTOK "$INSTDIR\voice-notes.exe.old"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog 'taskkill /F /IM "voice-notes.exe"'
  Sleep 400
!macroend
