# 设备电脑端并入 Voice Notes,固件仓库独立、协议以其文档为准

为了让同事在 Windows 上也能用 AI Passport 设备往编程 Agent(Orca、ChatGPT/Codex)里听写,我们把原本独立的 macOS 配套程序(vibe-voice-input 仓库的 `host/vibe-voice/`)整体搬进 Voice Notes,macOS 与 Windows 都只维护这一份;独立配套程序不再维护,搬完即删。这样 Windows 版直接继承 Voice Notes 已有的识别模型管理、托盘、安装包与一键更新,用户也只装一个程序。固件留在 vibe-voice-input 仓库(仓库不改名),设备与电脑之间的协议以该仓库的 `docs/vibe-voice/protocol.md` 为唯一标准,Voice Notes 照它实现;协议版本不匹配时明确提示哪一边需要升级,协议能不升版就不升版(同事的设备需要人工刷机)。

## Considered Options

- **共享核心库**(在固件仓库拆出平台无关的 crate,两边 git 依赖):协议只有一份实现,但要维护跨仓库版本钉,且独立配套程序要继续活着。既然独立程序要下线,共享库只剩成本。
- **Voice Notes 只当识别服务,配套程序自己做 Windows 版**:用户要装两个常驻程序,且 Voice Notes 的本地控制接口在 Windows 上还是空壳,托盘/安装包/更新都得在配套程序里再做一遍。

## Consequences

- 设备功能的发版与 Voice Notes 绑定,修一个设备 bug 也要走一次 Voice Notes 发布。
- 开发设备功能会频繁热重载,而录制中改代码会杀掉录制,所以开发版改用独立应用标识 `com.teemo.voice-notes.dev`,与日常录会的安装版分开数据和权限。
- Windows 上 btleplug 不会配对,设备又只接受加密+认证+绑定的连接,所以 Voice Notes 在应用内用 WinRT 自行配对(用户输入设备屏幕上的 6 位码);macOS 沿用系统配对弹框。
