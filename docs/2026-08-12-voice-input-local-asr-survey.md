# 语音输入法与录音备忘类应用的本地(端侧)ASR 调研

> 项目级调研文档。调研时间 2026-08,来源附于各节并汇总于文末。
> 口径约定:**"端侧"指识别计算本身在设备上完成**;"音频不上传但元数据/结果可能上传"与"识别在云端但承诺不留存"是不同的事,各节分开说明。来源性质标注:【官方】一手文档/【媒体】二手报道/【推断】无一手来源的合理推断。

## TL;DR

- **是的,主流移动端语音输入和系统级录音转写在 2021 年后已大规模转向端侧 ASR,但"端侧"都是有条件的**:特定芯片(苹果 A12+/谷歌 Tensor)、特定语言(苹果约 20 个语种 40+ locale、谷歌以英语为先)、特定产品线(Pixel 全端侧,非 Pixel 安卓默认云端)。
- **录音机/备忘录类**:Pixel Recorder 从 2019 年起完全端侧;苹果 iOS 18 语音备忘录/通话录音转写在 iPhone 12+ 端侧运行(含简繁中文);三星走"混合 AI",转写基础功能可端侧、摘要在云端,并提供"仅在设备端处理数据"总开关。
- **中文输入法生态相反:默认云端**。微信及微信输入法的语音转文字明确上传腾讯服务器(隐私政策原文);讯飞/百度/搜狗均以云端为默认、离线包为降级备选(20–30MB 小模型,准确率折损)。
- 对本项目(voice-notes,自带本地 ASR)的含义:全本地转写已是一线大厂的隐私卖点与既成事实,不是激进选择;差异化在"无芯片/语言门槛 + 会议级长音频 + 说话人识别"这些系统自带能力不覆盖的地方。

## 一、iOS 键盘听写(Dictation)与 Siri

**结论:混合 → 端侧为主。iOS 15(2021)起,A12 Bionic 及更新芯片 + 受支持语言的组合走端侧,其余组合仍回退苹果服务器。**

- **机型条件**【官方】:苹果支持文档列出支持端侧听写的机型为 iPhone XR/XS 及之后所有机型、iPhone SE 2 代及之后——即 **A12 Bionic(神经引擎)为最低门槛**;需先下载 Siri 语音模型,可在"设置 > Siri 与搜索"下看到"语音输入在 iPhone 上处理"字样确认。来源:[Models that support dictation on device](https://support.apple.com/gu-in/guide/iphone/aside/iphd22c7edd5/15.0/ios/15.0)
- **语言条件**【官方】:苹果 iOS Feature Availability 页面"Dictation: On-device and Modeless Dictation"列出约 48 个 locale,含普通话(中国大陆)、粤语(香港/中国大陆简体)、英日韩法德西俄阿等;**不在列表内的语言仍走服务器听写**。来源:[iOS Feature Availability](https://www.apple.com/ios/feature-availability/)
- **隐私口径的细微差别**【官方】:苹果法务页原文——键盘设置里会标明"音频与转写文本在设备上处理、不发送给苹果服务器";否则(不满足条件时)"你听写的内容会发送到服务器处理,但除非你选择加入 Improve Siri and Dictation 否则不存储"。**且即便端侧处理,苹果仍收集请求类别、是否成功、设备规格、性能统计等元数据**(不含音频)。另外**搜索框里的听写(Dictation in Search)始终走服务器**。来源:[Siri, Dictation & Privacy(法务原文)](https://www.apple.com/legal/privacy/data/en/ask-siri-dictation/) · [Apple 隐私功能页](https://www.apple.com/privacy/features/)("On-device dictation helps protect your privacy further by performing all processing completely offline";Siri:"The audio of your requests is processed entirely on your device unless you choose to share it with Apple")
- **时长限制**【官方+媒体】:iOS 15 前键盘听写有 60 秒上限(更早为 30 秒);iOS 15 起**端侧听写取消时长上限,可连续听写**(服务器回退路径仍有限制);但约 30 秒无语音静默仍会自动停止。来源:[iOS 15(Wikipedia,引苹果发布说明)](https://en.wikipedia.org/wiki/IOS_15) · 【媒体】[yaps.ai 实测](https://www.yaps.ai/blog/apple-dictation-stops-after-30-seconds)
- **开发者 API**【官方】:Speech framework 的 `SFSpeechRecognizer` 自 iOS 13 提供 `supportsOnDeviceRecognition`(能力查询)与 `SFSpeechRecognitionRequest.requiresOnDeviceRecognition`(强制端侧,音频不出设备;服务器路径则有单次约 1 分钟音频限制)。来源:[requiresOnDeviceRecognition](https://developer.apple.com/documentation/speech/sfspeechrecognitionrequest/requiresondevicerecognition)
- **iOS 26 新 API**【官方】:WWDC25 推出 `SpeechAnalyzer`/`SpeechTranscriber` 替代 SFSpeechRecognizer,**完全端侧**、面向长音频与实时流,模型由系统统一管理(AssetInventory 按语言下载);苹果自家 Notes/Voice Memos/通话转写用的就是这套模型。来源:[WWDC25 Session 277: Bring advanced speech-to-text to your app with SpeechAnalyzer](https://developer.apple.com/videos/play/wwdc2025/277/)

## 二、Apple 语音备忘录 / 通话录音 / Notes 音频转写(iOS 18+)

**结论:端侧(官方明示条件:iPhone 12 及更新机型 + 10 种语言,含简繁中文)。不依赖 Apple Intelligence;只有"摘要"这一步才需要 Apple Intelligence。**

- **条件**【官方】:"Audio transcription is available on iPhone 12 or later in English (all variants), Spanish, Portuguese, Italian, French, German, Japanese, Korean, Simplified Chinese, and Traditional Chinese"——语音备忘录、Notes 录音、iOS 18.1 通话录音三处转写条件一致;Mac 侧要求 Apple silicon + macOS Sequoia。来源:[View a Voice Memos transcription](https://support.apple.com/guide/iphone/view-a-transcription-iph00953a982/ios) · [Record and transcribe audio in Notes](https://support.apple.com/guide/iphone/record-and-transcribe-audio-iphbe11247b5/ios) · [Record and transcribe a call](https://support.apple.com/en-us/121583)
- **端侧证据链**:苹果支持文档未在每页逐句写"on device",但 (a) 机型门槛是 iPhone 12(A14)而非联网能力;(b) WWDC25 官方明确说这些系统 App 用的是 SpeechAnalyzer 端侧模型【官方】;(c) 飞行模式下转写可用【媒体实测】。综合判定为端侧,置信度高。
- **Apple Intelligence 的边界**【官方】:转写本身 iPhone 12 即可;**通话/录音的"摘要"才要求 Apple Intelligence**(iPhone 15 Pro/16 及更新)。这也说明"转写"与"生成式摘要"在苹果的产品切分里是两层。

## 三、Android / Google 语音输入

**结论:分裂。Pixel 系全端侧且为宣传点;非 Pixel 安卓默认云端,离线包是降级备选。**

- **2019 转折点**【官方】:Google AI Blog《An All-Neural On-Device Speech Recognizer》——RNN-T 端到端模型经量化压到 80MB,首发 **Gboard 美式英语、仅 Pixel**。这是移动端侧 ASR 的标志性落地。来源:[Google Research Blog 2019-03](https://ai.googleblog.com/2019/03/an-all-neural-on-device-speech.html)
- **Assistant voice typing(2021)**【官方】:Pixel 6 起,Gboard 的增强语音输入依赖 **Tensor 芯片**在端侧跑更大模型:自动标点、无超时连续听写、离线可用;Google 支持页明示"advanced voice typing 需 Pixel 6 及更新"。来源:[Use advanced voice typing features(Gboard Help)](https://support.google.com/gboard/answer/11197787?hl=en)
- **系统 API**【官方】:`SpeechRecognizer.createOnDeviceSpeechRecognizer()` 与 `isOnDeviceRecognitionAvailable()`(API 31/Android 12 起)强制端侧、无本地引擎则失败;更早的 `RecognizerIntent.EXTRA_PREFER_OFFLINE`(API 23)只是"偏好离线"提示,不保证。端侧引擎由系统组件"Speech Services by Google"(Android 12 起跑在 Private Compute Core 内)提供,**厂商可不预装或用自家引擎**。来源:[SpeechRecognizer 文档](https://developer.android.com/reference/android/speech/SpeechRecognizer) · [RecognizerIntent 文档](https://developer.android.com/reference/android/speech/RecognizerIntent)
- **非 Pixel 现实**【媒体+推断】:三星/一加等高通、联发科机型上,Gboard 语音输入**默认走谷歌服务器**,只有手动下载"离线语音识别"语言包或断网时才落到较小的本地模型(准确率下降);Tensor 级别的端侧大模型体验未开放给非 Pixel。此点无谷歌一手成文声明,依据为 Gboard 帮助页的离线包设计 + 媒体横评,标注为**推断(强)**。来源:【媒体】[Android Authority: Pixel's on-device voice typing is game-changing, so why can't everyone have it?](https://www.androidauthority.com/voice-typing-opinion-3221341/)

## 四、Pixel Recorder(Google 录音机)

**结论:完全端侧,官方宣传属实且有技术博客背书。**

- 【官方】Google Research 博客《The On-Device Machine Learning Behind Recorder》(2019,随 Pixel 4 发布):转写、声音事件分类、检索全部端侧,无需网络。来源:[research.google](https://research.google/blog/the-on-device-machine-learning-behind-recorder/)
- 【官方】2024 年 Android Developers Blog:Recorder 的"说话人标签+摘要"由 **Gemini Nano(端侧 LLM)**驱动,明确以"无网络、隐私、低延迟"为卖点。来源:[Android Developers Blog 2024-08](https://android-developers.googleblog.com/2024/08/recorder-app-on-pixel-sees-boost-in-engagement-with-gemini-nano.html)
- 限制:转写语言长期以英语等少数语言为主;摘要/说话人标签需 Pixel 8 Pro+(Gemini Nano 机型门槛)。

## 五、三星语音备忘录 / Galaxy AI Transcript Assist

**结论:混合("Hybrid AI"是三星官方定位)。转写(基础)可端侧,摘要/翻译多走云端;提供系统级"仅在设备端处理数据"开关,开启后转写仍可用、摘要等云端功能被禁用。**

- 【官方】三星编辑部文章将 Galaxy AI 定位为"端侧 AI + 云端 AI 的混合":[Human-Centric, Hybrid AI Opens Up New Possibilities](https://news.samsung.com/global/human-centric-hybrid-ai-opens-up-new-possibilities);通话实时翻译(Live Translate)因隐私被刻意做成端侧:[Samsung Research 访谈](https://news.samsung.com/global/interview-fast-lightweight-and-on-device-ai-how-samsung-research-built-ai-features-that-translate-in-real-time)
- 【官方】Knox 企业文档"Data processing for Galaxy AI":管理员可强制"仅端侧处理";并声明"端侧处理的 AI 功能不会用你的数据训练模型,云端处理的可能用于训练"。来源:[docs.samsungknox.com](https://docs.samsungknox.com/admin/knox-platform-for-enterprise/knox-service-plugin/configure-advanced-policies/data-processing-for-galaxy-ai/)
- 【官方】用户侧开关:设置 > Galaxy AI(或"高级智能")> "仅在设备端处理数据";三星英国支持页说明 Transcript Assist 覆盖录音机与 Notes 的转写/摘要/翻译。来源:[How to use Galaxy AI Transcript Assist(Samsung UK)](https://www.samsung.com/uk/support/mobile-devices/how-to-use-galaxy-ai-transcript-assist/)
- 【媒体】开启"仅端侧"后:录音转写用本地已下载语言包仍可用,**摘要因需云端而失效**;2026 年 One UI 9 起录音机新增"更强的云端转写"可选项(端侧/云端二选一)。来源:[9to5Google](https://9to5google.com/2025/02/20/how-to-turn-on-galaxy-ai-on-device-processing/) · [Android Headlines](https://www.androidheadlines.com/2026/07/samsung-voice-recorder-one-ui-9-cloud-ai-transcription.html)
- 机型/语言门槛:Galaxy S24 起(后下放部分 S23/S22),转写语言依赖本地语言包下载;具体语言清单三星未给出统一成文列表,**未证实**细节不展开。

## 六、中文生态:讯飞 / 百度 / 搜狗输入法、微信语音转文字

**结论:与欧美系统厂相反——默认云端,端侧是降级备选或根本没有。官方技术披露少,以下逐项标注来源性质。**

| 产品 | 口径 | 来源性质 |
|---|---|---|
| **微信"语音转文字"** | **云端**。微信隐私保护指引原文:使用语音转文字时"收集你的语音信息……实时处理后返回结果,不会保存"(实时处理≠端侧,识别引擎为腾讯"微信智聆"云端服务) | 【官方】[微信隐私保护指引](https://weixin.qq.com/cgi-bin/readtemplate?lang=zh_CN&t=weixin_agreement&s=privacy) |
| **微信输入法** | **云端,白纸黑字**。其隐私政策原文:"需上传你输入的语音信息至腾讯的服务器,经过实时处理后将向你返回文字结果,不会储存你输入的语音信息" | 【官方】[微信输入法隐私政策](https://z.weixin.qq.com/web/privacy) |
| **讯飞输入法** | **默认云端,官方提供可选"离线语音"包**(设置中手动开启并下载,分低端/中高端机型两档模型);讯飞官方宣传离线模式"本地解码、不传网络" | 【官方】离线功能实为官网/App 内置能力;开启教程见[讯飞官方知乎号](https://zhuanlan.zhihu.com/p/81840911);"打字数据不上传"表述见【媒体】转载,原始新闻稿未检索到,标注**推断(中)** |
| **百度输入法** | **默认云端**;内置约 30MB 离线语音模型,弱网/断网时**自动**切换(无独立开关);百度自称离线与在线准确率接近 | 【媒体】横评实测([搜狐横评](https://m.sohu.com/n/477646731/)、[知乎评测](https://zhuanlan.zhihu.com/p/514849828));无官方架构文档,标注**推断(中)** |
| **搜狗输入法** | **默认云端**;可下载约 22.5MB 离线语音包并选择触发条件(弱网/无 WiFi/始终);实测离线准确率明显下滑 | 【媒体】同上横评;标注**推断(中)** |

共性:中文输入法厂商的商业模式(热词、云端大模型、个性化)决定了云端优先;离线包是 2015 年代技术的小模型,与苹果/谷歌"端侧即主力"的路线本质不同。**没有一家在隐私政策中承诺语音识别默认端侧。**

## 七、Whisper 系端侧方案在消费级 App 中的采用(简述)

whisper.cpp(OpenAI Whisper 的 C/C++ 移植,支持 Apple Silicon/iOS/Android 端侧推理)已被多款备忘录/转写类 App 用作全本地引擎,有官方明示的例子:

- **MacWhisper / Whisper for iOS**(Jordi Bruin):官方文档明示"transcription fully offline / on-device model"(iOS 端侧模式要求 iPhone 15+)。来源:【官方】[docs.macwhisper.com](https://docs.macwhisper.com/article/33-macwhisper-for-ios) · [whisper.cpp 官方讨论区](https://github.com/ggml-org/whisper.cpp/discussions/420)
- **Aiko**(Sindre Sorhus):App Store 页面官方描述"powered by OpenAI's Whisper running locally on your device. Nothing leaves your device",常见用法是把系统语音备忘录分享给它做本地转写。来源:【官方】[App Store: Aiko](https://apps.apple.com/app/aiko/id1672085276)

这类 App 的存在说明:在系统自带转写的语言/机型门槛之外,"全本地 Whisper"已是成熟的消费级产品形态。

## 八、对照总结表

| 产品 | 默认口径 | 端侧条件 | 云端条件/回退 | 关键来源 |
|---|---|---|---|---|
| iOS 键盘听写 | 端侧(条件满足时) | A12+ 芯片、约 48 个 locale(含简中/粤语)、已下载语音模型;无时长上限 | 不支持的语言/机型回退苹果服务器(不留存,除非 opt-in);搜索框听写恒为服务器;端侧仍上传请求元数据 | [Apple 支持](https://support.apple.com/gu-in/guide/iphone/aside/iphd22c7edd5/15.0/ios/15.0) · [法务页](https://www.apple.com/legal/privacy/data/en/ask-siri-dictation/) |
| Siri 音频 | 端侧(音频) | iOS 15+,同芯片条件 | 请求语义仍可能上云;音频仅 opt-in 共享 | [Apple 隐私页](https://www.apple.com/privacy/features/) |
| 语音备忘录/通话/Notes 转写(iOS 18) | 端侧 | iPhone 12+,10 种语言(含简繁中文) | 无云端回退;摘要需 Apple Intelligence(机型更高) | [Apple 支持](https://support.apple.com/guide/iphone/view-a-transcription-iph00953a982/ios) |
| Gboard(Pixel) | 端侧 | Pixel(2019 起英语);增强版需 Pixel 6+/Tensor | 未覆盖语言走服务器 | [Google Research](https://ai.googleblog.com/2019/03/an-all-neural-on-device-speech.html) · [Gboard Help](https://support.google.com/gboard/answer/11197787?hl=en) |
| Gboard(非 Pixel) | **云端** | 手动下载离线包/断网降级(小模型) | 默认服务器识别 | 【媒体+推断】[Android Authority](https://www.androidauthority.com/voice-typing-opinion-3221341/) |
| Pixel Recorder | 端侧(完全) | Pixel;语言以英语为主;摘要需 Pixel 8 Pro+(Gemini Nano) | 无 | [research.google](https://research.google/blog/the-on-device-machine-learning-behind-recorder/) |
| 三星录音机/Transcript Assist | 混合 | 转写可端侧(本地语言包);"仅设备端处理"开关可强制 | 摘要/部分转写走云端;One UI 9 提供云端转写可选 | [Samsung Newsroom](https://news.samsung.com/global/human-centric-hybrid-ai-opens-up-new-possibilities) · [Knox 文档](https://docs.samsungknox.com/admin/knox-platform-for-enterprise/knox-service-plugin/configure-advanced-policies/data-processing-for-galaxy-ai/) |
| 微信/微信输入法语音转文字 | **云端**(隐私政策明示上传) | 无端侧模式 | 实时处理、承诺不留存 | [微信输入法隐私政策](https://z.weixin.qq.com/web/privacy) |
| 讯飞/百度/搜狗输入法 | **云端为默认** | 可选/自动离线小模型(20–30MB) | 在线识别为主力口径 | 【官方(讯飞离线功能)+媒体横评】 |
| whisper.cpp 系 App(MacWhisper/Aiko) | 端侧(完全) | 本地算力(Apple Silicon/新 iPhone) | 无(部分 App 另售云端加速选项) | [MacWhisper 文档](https://docs.macwhisper.com/article/33-macwhisper-for-ios) |

## 九、对本项目(voice-notes)的启示

1. **"全本地转写"已从差异化卖点变成一线厂的标配叙事**(苹果隐私页、Pixel Recorder、三星端侧开关都在讲这个故事),本项目不必再论证"本地可不可行",而应强调系统方案覆盖不到的组合:**无机型/芯片门槛 + 中文会议级长音频 + 说话人识别/跨会话认人**——苹果 10 语种转写没有说话人分离,Pixel Recorder 说话人标签锁死 Pixel 8 Pro+,这正是本项目的空档。
2. **"端侧"要说清口径,学苹果不学营销**:苹果即便端侧仍上传请求元数据,并把"搜索听写恒走服务器"写进法务页。本项目"所有数据均在本机"是比 Apple/三星更干净的口径(连元数据也不出机),对隐私敏感用户(会议场景)值得在文档里对照着讲。
3. **中文输入法生态默认云端**,意味着中文用户的"语音→文字"心智基本建立在云端识别的准确率上;本项目的本地 ASR 会被拿来跟讯飞/微信云端比准确率,而不是跟苹果端侧比——ASR 准确率提升(见 `asr-system-improvement-plan.md`)的对标对象应该是云端中文引擎,预期差距要在产品里管理(如置信度提示、重转写)。

## 参考来源

**Apple(官方)**
- 端侧听写机型列表:https://support.apple.com/gu-in/guide/iphone/aside/iphd22c7edd5/15.0/ios/15.0
- 听写使用指南:https://support.apple.com/guide/iphone/dictate-text-iph2c0651d2/ios
- Siri, Dictation & Privacy 法务原文:https://www.apple.com/legal/privacy/data/en/ask-siri-dictation/
- 隐私功能页:https://www.apple.com/privacy/features/
- iOS Feature Availability(端侧听写语言列表):https://www.apple.com/ios/feature-availability/
- 语音备忘录转写:https://support.apple.com/guide/iphone/view-a-transcription-iph00953a982/ios
- Notes 录音转写:https://support.apple.com/guide/iphone/record-and-transcribe-audio-iphbe11247b5/ios
- 通话录音转写:https://support.apple.com/en-us/121583
- requiresOnDeviceRecognition:https://developer.apple.com/documentation/speech/sfspeechrecognitionrequest/requiresondevicerecognition
- WWDC25 SpeechAnalyzer:https://developer.apple.com/videos/play/wwdc2025/277/

**Google(官方)**
- An All-Neural On-Device Speech Recognizer(2019):https://ai.googleblog.com/2019/03/an-all-neural-on-device-speech.html
- Recorder 端侧 ML(2019):https://research.google/blog/the-on-device-machine-learning-behind-recorder/
- Recorder × Gemini Nano(2024):https://android-developers.googleblog.com/2024/08/recorder-app-on-pixel-sees-boost-in-engagement-with-gemini-nano.html
- Gboard 高级语音输入帮助:https://support.google.com/gboard/answer/11197787?hl=en
- SpeechRecognizer API:https://developer.android.com/reference/android/speech/SpeechRecognizer

**Samsung(官方)**
- Hybrid AI 编辑部文章:https://news.samsung.com/global/human-centric-hybrid-ai-opens-up-new-possibilities
- 端侧翻译研发访谈:https://news.samsung.com/global/interview-fast-lightweight-and-on-device-ai-how-samsung-research-built-ai-features-that-translate-in-real-time
- Knox Galaxy AI 数据处理策略:https://docs.samsungknox.com/admin/knox-platform-for-enterprise/knox-service-plugin/configure-advanced-policies/data-processing-for-galaxy-ai/
- Transcript Assist 使用说明:https://www.samsung.com/uk/support/mobile-devices/how-to-use-galaxy-ai-transcript-assist/

**中文厂商(官方)**
- 微信隐私保护指引:https://weixin.qq.com/cgi-bin/readtemplate?lang=zh_CN&t=weixin_agreement&s=privacy
- 微信输入法隐私政策:https://z.weixin.qq.com/web/privacy
- 讯飞输入法离线语音开启教程(讯飞官方知乎号):https://zhuanlan.zhihu.com/p/81840911

**媒体/二手(已标注)**
- Android Authority(非 Pixel 端侧缺失):https://www.androidauthority.com/voice-typing-opinion-3221341/
- 9to5Google(三星仅端侧开关实测):https://9to5google.com/2025/02/20/how-to-turn-on-galaxy-ai-on-device-processing/
- Android Headlines(One UI 9 云端转写):https://www.androidheadlines.com/2026/07/samsung-voice-recorder-one-ui-9-cloud-ai-transcription.html
- 输入法离线语音横评:https://m.sohu.com/n/477646731/
- yaps.ai(听写静默超时实测):https://www.yaps.ai/blog/apple-dictation-stops-after-30-seconds

**whisper.cpp 生态(官方)**
- MacWhisper 文档:https://docs.macwhisper.com/article/33-macwhisper-for-ios
- Aiko(App Store 官方描述):https://apps.apple.com/app/aiko/id1672085276
- whisper.cpp:https://github.com/ggml-org/whisper.cpp
