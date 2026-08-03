# macOS 代码签名与公证

## 为什么需要

未签名(ad-hoc)的应用,macOS 只能拿 **CDHash**(二进制内容指纹)当身份来记录权限授权。
二进制只要重新构建一次指纹就变,系统即视为另一个应用:

- 每次本地重新构建,麦克风/屏幕录制权限都要重新授权一遍;
- 系统大版本升级重建 TCC 数据库时,授权同样失效——「隐私与安全性」列表里还留着
  同名的旧条目,但绑的是对不上号的旧指纹,于是「明明在列表里却还要重新授权」;
- 分发给别人时 Gatekeeper 直接拦截,用户必须手工 `xattr -dr com.apple.quarantine`。

用 Developer ID 证书签名后,授权绑定在**开发者身份**(Team ID + bundle id)上,
重新构建与系统升级都不再掉权限。再经过公证(notarization),其他用户双击即可打开。

## 本地构建(已配置)

`src-tauri/tauri.conf.json` 的 `bundle.macOS.signingIdentity` 指向 Developer ID 证书。
`npm run tauri build` 会自动签名,无需额外操作。

验证签名:

```bash
codesign -dv --verbose=2 src-tauri/target/release/bundle/macos/voice-notes.app
# 期望看到 Authority=Developer ID Application: ... 与 TeamIdentifier=<你的 Team ID>
# flags 里应含 runtime(Hardened Runtime,公证的前置条件)
```

### 换一台机器 / 换一个开发者

证书是绑定个人的,别人 clone 本仓库后没有这张证书,构建会失败。两种做法:

```bash
# 一次性:用自己的身份构建
APPLE_SIGNING_IDENTITY="Developer ID Application: 你的名字 (TEAMID)" npm run tauri build

# 或者:不签名(退回 ad-hoc,权限会掉但能跑通)
APPLE_SIGNING_IDENTITY="-" npm run tauri build
```

环境变量优先级高于配置文件,这也是 CI 采用的方式。

## 公证(分发给他人时必需)

签名只解决「本机权限稳定」;要让**别人**下载后能直接打开,还需要公证。

### 一次性准备

1. 到 [appleid.apple.com](https://appleid.apple.com) → 登录和安全 → App 专用密码,生成一个。
2. 存进钥匙串(密码不落地到任何文件或命令历史之外的地方):

```bash
xcrun notarytool store-credentials voice-notes-notary \
  --apple-id "你的 Apple ID 邮箱" \
  --team-id "你的 Team ID" \
  --password "刚生成的 App 专用密码"
```

### 每次发布

```bash
npm run tauri build            # 产出已签名的 .app / .dmg
./scripts/notarize_macos.sh    # 提交公证 → staple → 验证,一条龙
```

脚本做的三件事:`notarytool submit --wait` 提交并等结果、`stapler staple` 把票据钉进包
(分发前必做,否则用户断网时仍会被拦)、`spctl` 验证。期望最后看到
`accepted / source=Notarized Developer ID`。

手工执行等价于:

```bash
DMG=$(ls -t src-tauri/target/release/bundle/dmg/*.dmg | head -1)
xcrun notarytool submit "$DMG" --keychain-profile voice-notes-notary --wait
xcrun stapler staple "$DMG"
spctl -a -vv -t open --context context:primary-signature "$DMG"
```

公证通常几分钟内完成。若被拒,用返回的 submission id 查原因:

```bash
xcrun notarytool log <submission-id> --keychain-profile voice-notes-notary
```

常见拒因是 Hardened Runtime 未开或嵌套二进制未签名——本项目的
`Entitlements.plist` 与 Tauri 的签名流程已覆盖这两点。

已实测:v0.7.0 的 DMG 于 2026-08-03 公证通过(status: Accepted),staple 与 spctl 均放行。

## CI 发布(已接入)

`.github/workflows/release.yml` 从 Environment「TAURI」注入六项 Apple secrets,
tauri-action 会建临时钥匙串、导入证书、签名并公证 `.app`。

**注意 tauri-action 只公证 `.app`,不公证 dmg**:它把已公证的 app 打进 dmg 后就结束了,
dmg 自身既无公证也无票据。表现是 app 拖出来能跑,但用户双击 dmg 挂载那一刻仍会看到
「无法验证开发者」。v0.8.0 首次接入时实测到这点(dmg `rejected/Unnotarized`,内部 app
`accepted/Notarized`),故 workflow 里补了一步单独公证 dmg → staple → 用 spctl 断言
`source=Notarized Developer ID`,不符即让发布失败,再覆盖 Release 里的同名资产。

发版后值得下载真实产物验一遍(日志里的 "Notarizing Accepted" 只代表 app):

```bash
gh release download vX.Y.Z --pattern '*.dmg'
xattr -w com.apple.quarantine "0083;00000000;Safari;" voice-notes_X.Y.Z_aarch64.dmg
spctl -a -vv -t open --context context:primary-signature voice-notes_X.Y.Z_aarch64.dmg
# 期望 accepted / source=Notarized Developer ID
```

### 首次配置(已完成,存档备查)

`.github/workflows/release.yml` 目前用 `APPLE_SIGNING_IDENTITY: "-"` 退回 ad-hoc,
因此 **Release 里的安装包仍是未签名的**,用户首次打开仍需 `xattr` 去隔离属性。

要接入,需在仓库 Secrets 配置以下项(tauri-action 原生支持):

| Secret | 取得方式 |
| --- | --- |
| `APPLE_CERTIFICATE` | 钥匙串导出 Developer ID 证书为 `.p12` 后 `base64 -i cert.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | 导出 `.p12` 时设置的密码 |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: 你的名字 (TEAMID)` |
| `APPLE_ID` | Apple ID 邮箱 |
| `APPLE_PASSWORD` | App 专用密码 |
| `APPLE_TEAM_ID` | Team ID |

配好后可用 `gh workflow run apple-signing-check.yml` 自检:验六项非空、证书可导入且
身份名匹配、公证凭据能通过 Apple 鉴权。tauri-action 在 secrets 缺失时**不会让构建失败**,
只静默退回 ad-hoc,故发版前跑一次自检比事后查日志可靠。

**注意**:`.p12` 与专用密码等同于代码签名私钥,一旦泄露他人即可冒用你的身份签名。
只放进仓库 Secrets,不要提交进仓库,也不要贴进任何对话或 issue。
