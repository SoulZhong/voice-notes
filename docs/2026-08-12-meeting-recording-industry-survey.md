# 会议录音与转写产品的音频采集与 ASR 架构调研

> 项目级调研文档。调研时间 2026-08,来源附于各节并汇总于文末。
> 口径约定:**"逐流归属"指说话人标签来自参会者音频流的信令身份(user_id/账号显示名),"声纹 diarization"指从混合音频中靠声学特征事后拆分**——两者是本质不同的机制,本文全程区分。来源性质标注:【官方】一手文档/【论文】学术一手/【媒体】二手报道/【推断】无一手来源的合理推断。
> 姊妹篇:`2026-08-02-speaker-recognition-industry-survey.md`(声纹/diarization 技术侧)、`2026-08-12-voice-input-local-asr-survey.md`(端侧 ASR)。本项目系统现状见 `asr-pipeline.md` / `speaker-identification-architecture.md`。

## TL;DR

1. **VoIP 会议转写"准"的根本原因不在 ASR 模型,在输入信号与元数据**:每个参会者的上行音频天然是独立、近场(贴嘴麦克风)、客户端已做 AEC+降噪的单人流;服务端按流的信令身份(user_id/显示名)打说话人标签,归属**零成本且天然正确**。Zoom RTMS 官方文档明说音频"per participant"、转写"per participant with attribution";Teams 转写自带 "the speaker's name"。
2. **声纹在 VoIP 会议里只是补丁,不是主线**:只有"多人共用一只麦克风"(会议室设备/同设备围坐)时才需要——Teams Rooms voice profile、腾讯会议"识别不同发言人"、飞书妙记"重新识别说话人"三家的声纹功能全部定位为**同设备多人场景的事后补救**,且都要手动触发或额外许可。反证:腾讯/飞书官方都承认"同一台设备多人发言会被记为同一个人"——若是声纹方案不会有此缺陷。
3. **会议笔记产品分三种采集架构**:①bot 入会(Otter/Fireflies)——以虚拟参会者身份进会,能拿平台参会者元数据;②单机双轨 mic+系统回环(Granola、Krisp、本项目 voice-notes)——Granola 官方自认天然上限是 **"Me vs Them"**,逐人认名要靠平台集成补名;③硬件拾音(Plaud、讯飞录音笔)——多麦阵列远场采声,云端 diarization 匿名标号("说话人 1/2")。
4. **远近场差距是量级而非几个点**:同一 ASR 系统,CHiME-5 佩戴麦 WER 47.9% vs 2 米外阵列 81.3%(绝对差 33.4pp);Kaldi AMI 官方基线近讲 25.5% vs 单远场麦 49.3%(近乎翻倍);远场多人+自动说话人归属的联合任务(CHiME-7)基线错误率仍在 50%+。声纹 diarization 在真实会议 DER 就是 13–23% 量级。
5. **对本项目的含义**:单机双轨的四大难点(远场 mic 轨、回放串入需 AEC、双轨时钟不同源、说话人只能声纹事后分)每一条都有工业/学术一手来源背书,也都被本项目亲历过(AEC 参考迟到、mic 轨时基漂移)。同样的 ASR 模型,VoIP 逐流方案赢在起跑线上——这是物理与架构差距,不是模型差距;本项目的差异化只能建立在"不入会、全本地、跨会议认人"这些 VoIP 方案做不了或不愿做的事上。

## 一、线上会议软件:逐流采集 + 云端 ASR + 信令归属

### 1.0 机制总论

VoIP 会议的音频拓扑决定了转写质量:每个参会者用自己的设备近讲拾音,客户端上行前完成回声消除/降噪(两家官方均有成文),SFU/MCU 服务端**始终持有 N 路带身份的独立流**。转写服务消费的是"干净近场单人流 + 谁在说的信令事实",与"从一段混合录音里猜谁在说"是两个难度级别的问题。

客户端前处理的一手证据:

- 【官方】Zoom:"By default, the Zoom app will utilize **noise suppression and echo cancellation** to improve the quality of the audio received by your microphone."(降噪四档,High 档连他人背景说话声都消;专业音频模式才关闭前处理)。来源:[Zoom KB0059985](https://support.zoom.com/hc/en/article?id=zm_kb&sysparm_article=KB0059985)
- 【官方】Teams:默认 Auto 档自适应降噪,High 档 "suppresses all background sound that isn't speech."。来源:[Reduce background noise in Teams meetings](https://support.microsoft.com/en-us/office/reduce-background-noise-in-microsoft-teams-meetings-1a9c6819-137d-4b3b-a1c8-4ab20b234c0d)
- AGC:两家支持页均未见对自动增益的独立成文描述(【未找到一手来源】);但 WebRTC 标准音频前处理链(AEC/NS/AGC 三件套)是行业事实,W3C getUserMedia 约束即含 `echoCancellation`/`noiseSuppression`/`autoGainControl` 三项。来源:[W3C Media Capture and Streams](https://www.w3.org/TR/mediacapture-streams/)

### 1.1 Zoom

**逐参会者原始音频是官方一手能力**:

- 【官方】Meeting SDK Raw Data:C++ 回调接口 `IZoomSDKAudioRawDataDelegate` 同时提供 `onMixedAudioRawDataReceived(AudioRawData*)`(全场混音)与 **`onOneWayAudioRawDataReceived(AudioRawData*, uint32_t user_id)`(按 user_id 逐参会者单路 PCM)**。即 SDK bot 可拿到每个人未混音的独立音频流。来源:[Zoom Meeting SDK Windows API Reference](https://marketplacefront.zoom.us/sdk/meeting/windows/class_i_zoom_s_d_k_audio_raw_data_delegate.html)(注:developers.zoom.us 的 raw-data 指南页对非浏览器抓取返回 404,逐参会者能力以上述 API 参考手册为准)
- 【官方】RTMS(Realtime Media Streams):"a data pipeline that gives your app access to live audio, video, and transcript data from Zoom meetings",经 WebSocket 提供 **per-participant** 的音频/视频/转写/聊天/共享屏幕流,无需再往会里塞 bot。音频"available **per participant** and as a merged packet"(合流包 user_id 为 0);转写"available **per participant with attribution**, also called diarization",数据包携带 `user_id`、`user_name`、`timestamp`、`language`。**归属键是参会者身份,不是声纹**。来源:[RTMS docs](https://developers.zoom.us/docs/rtms/) · [RTMS media](https://developers.zoom.us/docs/rtms/meetings/media/) · [发布博客](https://developers.zoom.us/blog/realtime-media-streams/)

**云录制 Audio transcript = 云端 ASR**:

- 【官方】"Audio transcription automatically transcribes the audio of a meeting or webinar that you record **to the cloud**",完成后以 VTT 文件出现在云录制列表,按时间戳分段、带说话人标签,存在 "Unknown Speaker" 兜底且可手工改名。来源:[Zoom KB0064927](https://support.zoom.com/hc/en/article?id=zm_kb&sysparm_article=KB0064927)
- 说话人名字来自参会者账号显示名而非声纹:官方支持页无一句明说,**标注【推断(强)】**——机制侧证:RTMS 转写归属用 user_id/user_name;Zoom 无参会者声纹注册功能;社区讨论佐证(【媒体】[Zoom Community](https://community.zoom.com/meetings-2/meeting-recordings-captions-speaker-attribution-12333))。

### 1.2 Microsoft Teams

**转写按账号身份归属**:

- 【官方】"The text appears alongside the meeting video or audio in real time, **including the speaker's name** and time stamp";且 "Participants can choose to **hide their identities** in meeting captions and transcripts"——能"隐藏身份"本身就说明归属来自账号身份而非声学特征。来源:[View live transcription in Teams meetings](https://support.microsoft.com/en-us/office/view-live-transcription-in-microsoft-teams-meetings-dc1a8f23-2e20-4684-885e-2152e06a4a8b)

**媒体 bot 的 unmixed audio(上限 4 路)**:

- 【官方】应用托管媒体 bot "has raw access to the voice, video, and screen sharing media streams";"**Active speakers** identify which participants are being heard in each received audio frame"(16kHz/16bit、20ms 帧)。来源:[Real-time media concepts](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/calls-and-meetings/real-time-media-concepts)
- 【官方】`AudioSocketSettings.ReceiveUnmixedMeetingAudio`:"receive **separate unmixed audio buffers for individual speakers**... up to **four** audio buffers, each corresponding to the **top four active speakers**";每路 `UnmixedAudioBuffer` 自带 `ActiveSpeakerId`(音源 ID,来自信令)。注意:是最活跃 4 路,不是全员逐路——与 Zoom 的全员 one-way 有差别。来源:[AudioSocketSettings](https://microsoftgraph.github.io/microsoft-graph-comms-samples/docs/bot_media/Microsoft.Skype.Bots.Media.AudioSocketSettings.html) · [UnmixedAudioBuffer](https://microsoftgraph.github.io/microsoft-graph-comms-samples/docs/bot_media/Microsoft.Skype.Bots.Media.UnmixedAudioBuffer.html)

**声纹(voice profile)只在会议室场景登场**——这是"逐流归属为主、声纹为补丁"最直白的官方文本:

- 【官方】"In a hybrid meeting, **without speaker recognition**, the video and audio feed for people in the room would be attributed to **the space (for example, Conference Room 1), not the individuals speaking**";Teams Rooms 用声纹("create a voiceprint for each participant")解决的正是"一路会议室流里混多人"的问题。需 Teams Rooms Pro / Teams Premium 许可,用户在桌面端注册 voice profile,未注册者显示 "Speaker 1/2"。来源:[Teams Rooms voice recognition](https://learn.microsoft.com/en-us/microsoftteams/rooms/voice-recognition) · [Intelligent Speakers](https://support.microsoft.com/en-us/teams/calls-devices/use-microsoft-teams-intelligent-speakers-to-identify-in-room-participants-in-a-meeting-transcription)

### 1.3 腾讯会议

- 【官方】云录制转写在云端:"会中云录制时,同时开启录制转写,会议语音可转文本,并能**展示对应参会者的发言内容**……支持云上编辑";逐字稿可会后在云录制详情页补生成。来源:[帮助 topic 1870](https://meeting.tencent.com/support/topic/1870/index.html) · [topic 1868](https://meeting.tencent.com/support/topic/1868/index.html)
- 【官方】**默认逐流归属,声纹是同设备多人时的补救**:"识别不同发言人"功能的适用条件写明——"当会中存在**多人使用同一台设备**发言,未曾在文本中显示出来时",可选参会者、设定发言人数后**重新生成**文本(仅桌面端、手动触发)。若是声纹方案不会存在"同设备被合并"这一缺陷。来源:[topic 1868](https://meeting.tencent.com/support/topic/1868/index.html) · [腾讯云文档 53483](https://cloud.tencent.com/document/product/1095/53483)
- 【官方】智能录制"按发言人维度只看他";"发言人总结**不包含开麦但没有发言的成员**"——参会者流内做 VAD,标签挂在参会者身份上。纪要生成模型可选 Hunyuan/DeepSeek。来源:[topic 1985](https://meeting.tencent.com/support/topic/1985/index.html)
- 【官方】转写语言:中/英/中英自动识别,"暂不支持方言识别"。来源:[腾讯云文档 76740](https://cloud.tencent.cn/document/product/1095/76740)
- ASR 引擎归属:官方唯一点名的是"**天籁实验室**"("在天籁实验室智能语音语义技术的加持下……实时转写能够精准识别发言",[官网新闻](https://meeting.tencent.com/news/bbsx241114.html));"背后是腾讯智聆/腾讯云 ASR"**未找到一手来源**,属【媒体/社区】说法,勿当事实引用。

### 1.4 飞书妙记(Lark Minutes)

- 【官方】云端转写、按版本计费限额:"文件将自动转写、处理并保存在妙记中","飞书基础版……提供一定的妙记语音转文字体验额度";语言"自动识别中文、英语、日语等"。来源:[生成妙记](https://www.feishu.cn/hc/zh-CN/articles/386045971891)
- 【官方】**会议场景默认逐流归属**,官方帮助页直接写出缺陷与两条路径的分界:"当会议中有多位参与者通过**同一台个人设备**……发言,录制生成的妙记可能会将其记录为**同一个说话人**";而"若多位参与者是通过**同一个会议室设备**进行发言……会**自动识别并拆分**说话人";"**录音和上传的音频文件**生成的妙记会自动识别并拆分说话人"。——个人设备=流归属出真名;会议室设备/本地录音单流=声纹 diarization 出"说话人 1/2"(可手动改名,仅 30 天内、桌面端;FAQ 声明拆分过程"不会存储用户的声纹信息")。来源:[在妙记中重新识别说话人](https://www.feishu.cn/hc/zh-CN/articles/812241214493)

### 1.5 钉钉闪记 / AI 听记

- 【官方】闪记定位为"视频会议音视频转文字",录后点"智能识别"云端异步转写,再生成智能摘要。来源:[钉钉帮助](https://www.dingtalk.com/qidian/help-detail-1060922251.html)
- 说话人区分:官方营销页称"声音识别技术自动辨别不同的讲话者"(注意该页脚注"以上内容由通义千问生成",证据强度低);【媒体】评测称"根据不同音色的发言人进行识别"——即声纹 diarization。**"与讯飞听见合作"未获任何官方佐证**;更可能由阿里自家通义听悟支撑(听悟官方文档明确支持"钉钉小程序"形态),引擎归属标【推断】。来源:[钉钉内容页](https://www.dingtalk.com/qidian/page-IgsRJiYB.html) · [什么是通义听悟](https://help.aliyun.com/zh/tingwu/what-is-tingwu)

## 二、会议笔记 / 录音硬件产品

### 2.1 Otter.ai:bot 入会 + 云端 ASR + 声纹学习式认人

- 【官方】采集两条路:App/浏览器直接拾音,或 **OtterPilot/Otter Notetaker bot 以访客身份通过会议链接加入** Zoom/Meet/Teams("can only join as a guest and cannot sign into any... account")实时转写。来源:[OtterPilot Overview](https://help.otter.ai/hc/en-us/articles/4425393298327-OtterPilot-Overview) · [Manually add Otter Notetaker](https://help.otter.ai/hc/en-us/articles/13676219922711-Manually-add-Otter-Notetaker-to-a-meeting)
- 【官方】说话人识别是**跨会议声纹学习式**:"You can manually tag speakers to help **Otter learn the speaker's voices**... Each time you tag a speaker, Otter will be able to recognize that speaker in future conversations";"learns from just a few tagged paragraphs for each speaker"。与本项目"被动积累+人工整理"路线同型(合规代价见姊妹篇:Otter 因被动采集声纹吃过 BIPA 集体诉讼)。来源:[Speaker Identification Overview](https://help.otter.ai/hc/en-us/articles/21665587209367-Speaker-Identification-Overview) · [Best Practices](https://help.otter.ai/hc/en-us/articles/37817241040535-Best-Practices-to-Maximize-Speaker-Identification)
- 【官方】全云端:数据存 AWS S3(SSE 加密),脱敏后用于训练自研模型;无本地转写选项。来源:[Privacy & Security](https://otter.ai/privacy-security)

### 2.2 Plaud Note / NotePin:硬件双拾音引擎 + 云端转写

- 【官方】"World's First to integrate a **dual-pickup engine**":面谈用空气传导麦克风(初代双麦;Note Pro 四 MEMS 麦 + AI 波束成形,宣称 5m 拾音),手机通话贴机背用**振动传导传感器(VCS)**直接读扬声器振动——物理层绕开系统禁止通话录音的 API 限制;通话录音"不支持戴耳机"证实是纯振动拾取。来源:[Plaud Note 产品页](https://www.plaud.ai/products/plaud-note-ai-voice-recorder) · [耳机限制 FAQ](https://support.plaud.ai/hc/en-us/articles/50837313885337-Can-I-use-headphones-earphones-when-I-record-a-phone-call-with-Plaud-Note)
- 【官方】设备只录音存储,转写/摘要全在 App 上传云端;早期官网明示"integrates OpenAI's **Whisper** STT model"(59 语言),现改称 112 语言多模型云服务(GPT-5/Claude/Gemini 做摘要)。说话人分离只承诺匿名 "Speaker labels"("know who said what"),不宣称跨录音认人。来源:[plaud.ai](https://www.plaud.ai/) · 旧文案转载【媒体】[Wellbots](https://www.wellbots.com/products/plaud-smart-ai-voice-recorder-audio-recorder)

### 2.3 讯飞听见 / 讯飞智能录音笔:多麦阵列 + 云端为主、端侧兜底

- 【官方】讯飞听见云端转写:"多语种转写翻译,准确率高达98%"(注明第三方检验所评测口径)、"1小时音频最快5分钟出稿,边录边转";"精准区分说话人"(角色分离标号,不宣称认名)。来源:[iflyrec.com](https://www.iflyrec.com/) · [帮助中心](https://www.iflyrec.com/helpCenter_guide/helpCenter_guide.html)
- 硬件:SR501 官方商城页称"8 颗哈曼麦克风";SR702 的"2+6 定向+矩阵麦克风阵列、15 米拾音、离线转写约 95%"出自【媒体】评测(官网详情为图片不可抓取)。录音笔文件同步讯飞听见 App 查看,转写后端共用【推断】。来源:[讯飞商城 SR501](https://www.xunfei.cn/goods.php?id=33) · 【媒体】[极客公园 SR702 评测](https://www.geekpark.net/news/316001)

### 2.4 Granola:与本项目同构的"单机双轨"——官方自认 "Me vs Them" 上限

与 voice-notes 架构最接近的对照,官方文档句句可引:

- 【官方】**无 bot、双轨采集**:"There is **no meeting bot** — Granola runs only on your computer and uses your **system audio and microphone**."
- 【官方】**不存音频、实时流式送云端第三方 ASR**:"passes audio directly from your microphone and system audio to our transcription provider... It does **not record or save audio**";"uses best-in-class transcription providers (like **Deepgram and Assembly**)"。Granola 自己不做 ASR。
- 【官方】**说话人区分 = 按轨分"我/他们"**:灰色气泡="transcription from **system audio**, usually other people in your meeting",绿色气泡="transcription from **your microphone**";要接入 Google Meet/Zoom 平台元数据才能"label who said what... using each participant's **display name**, instead of just **Me and Them**"。——官方明确承认:单靠 mic+系统回环双轨,说话人天然只能二分;逐人认名靠平台集成补名,**不靠声纹**。
- 【官方】手动启动,不自动入会/偷录。

来源:[Granola transcription docs](https://docs.granola.ai/help-center/taking-notes/transcription) · [Granola Security](https://www.granola.ai/security)

### 2.5 其他单机/不入会玩家(旁证)

- **Krisp AI Meeting Assistant**【官方】:bot-free;以虚拟麦克风/虚拟扬声器插入通话链路,天然拿到"我(mic)/对方(app 下行)"两路;官方承认若不设置 Krisp Speaker 则"only your speech will be transcribed";远端混音一路内多讲话人仍靠 diarization 标 Speaker N;宣称自研端侧 ASR。来源:[Krisp blog](https://krisp.ai/blog/krisps-ai-meeting-assistant-transcription-notes/) · [Krisp Mic/Speaker 原理](https://help.krisp.ai/hc/en-us/articles/4402174576402-How-Krisp-Microphone-and-Krisp-Speaker-work)
- **Notta**【官方】:云端 diarization 匿名标号("Speaker 1/2"格式),不宣称认名。来源:[Notta FAQ](https://support.notta.ai/hc/en-us/articles/4403163792027-Does-Notta-support-speaker-identification)
- **Tactiq**【官方】:第三条路线——不采音频,直接读 Google Meet 实时字幕("Instead of capturing audio, it reads the captions Google Meet already generates"),说话人名来自 Meet 字幕,精确但绑死平台。来源:[Tactiq](https://tactiq.io/learn/how-to-live-transcribe-google-meet-calls)
- **通义听悟**【官方】:纯云服务(网页/钉钉小程序/浏览器插件),依托千问与音视频 AI 模型;官方术语"说话人分离",FAQ 直白承认"**不能区别身份**,识别不同的发音人,可在会后修改发音人名称"——纯声纹聚类、匿名标号、人工改名,与腾讯会议/飞书会议场景"流归属出真名"形成本质对照。"实时说话人分离"2024-08 才上线。网页版"记录电脑内音频+麦克风"的双轨细节页需登录未能取证,标【推断/待验证】。来源:[什么是通义听悟](https://help.aliyun.com/zh/tingwu/what-is-tingwu) · [FAQ](https://help.aliyun.com/zh/tingwu/faq-about-basic-usage) · [产品动态](https://help.aliyun.com/zh/tingwu/release-notes)

## 三、线下远场拾音的工业实践(简要)

工业界从不裸用"一支麦收到什么算什么"的远场音频,标配是**阵列波束成形 + AEC + 降噪 + AGC/自动混音**四件套打包进设备 DSP:

- 【官方】Shure MXA920 天花阵列:"Steerable Coverage™ technology to precisely aim **8 pickup lobes with individual audio outputs**"(可转向波束、每波束独立输出);"Onboard IntelliMix® DSP applies **automatic mixing, echo cancellation, noise reduction, and automatic gain control**"。来源:[MXA920](https://www.shure.com/en-US/products/microphones/mxa920)
- 【官方】HP Poly:NoiseBlockAI(AI 滤非人声)+ Acoustic Fence(多麦空间围栏,滤界外人声)。来源:[Poly audio innovations](https://www.hp.com/us-en/poly/learning-hub/audio-innovations.html)(细节以 [solution brief PDF](https://h20195.www2.hp.com/v2/GetPDF.aspx/c09038815) 为准)
- 【官方】Logitech RightSound:波束成形(Rally Bar 6 MEMS 麦 5 自适应波束)+ 自动电平 + 降噪 + AEC,且明说 AEC 原理是"identifies (sounds) repeated within 128 milliseconds... then **filters the redundant sound**"(从麦克风信号里减掉扬声器串入)。来源:[RightSound](https://support.logi.com/hc/en-001/articles/360023206834-Understanding-RightSound-Technology) · [技术简报 PDF](https://download01.logitech.com/web/ftp/pub/pdf/cameras/rightsound_innovation_brief_enu.pdf)

**即便如此,远场 vs 近场的 WER 差距仍是量级**(数字均核实到原始论文/官方仓库):

| 基准 | 近场 | 远场 | 来源 |
|---|---|---|---|
| CHiME-5 dinner party(同一 LF-MMI TDNN)| 佩戴双耳麦 **47.9%** | 2m 外 Kinect 阵列(已波束成形)**81.3%**(绝对差 33.4pp,论文明言"main difficulty... comes from the source and microphone distance")| 【论文】[arXiv:1803.10609](https://arxiv.org/abs/1803.10609) |
| Kaldi AMI 官方 recipe(同一 TDNN)| IHM 头戴近讲 eval **25.5%** | SDM 单远场麦 eval **49.3%** | 【官方】[kaldi/egs/ami](https://github.com/kaldi-asr/kaldi/tree/master/egs/ami) |
| CHiME-6(远场多人)| — | Track 1(给定分段)基线 **51.3%**、冠军 ~30.5%;Track 2(需自己分说话人)基线 **77.9%**、冠军 ~42.7% | 【官方】[结果页](https://chimechallenge.github.io/chime6/results.html) |
| CHiME-7 DASR(转写+归属联合)| — | 基线 oracle 分段 33.4%、真实条件 **55.3%** | 【论文】[arXiv:2306.13734](https://arxiv.org/abs/2306.13734) |
| 微软 90 万小时预训练多说话人端到端(2021)| — | AMI-SDM **21.2%**(此前 SOTA 36.4%)——大厂堆料后远场仍远差于近讲典型水平 | 【论文】[arXiv:2103.16776](https://arxiv.org/abs/2103.16776) |

## 四、"单机双轨(mic + 系统回环)"的先天难点

本项目 voice-notes 与 Granola/Krisp 同构:mic 轨录本人+房间,系统回环轨录远端混音。逐条列出结构性难点及支撑来源:

**1. mic 轨是远场轨:房间混响、多人交叠、环境噪声。**
线下多人围坐时 mic 轨就是上表的"远场"条件(WER 翻倍量级,见第三节)。重叠语音口径(【论文】核实,注意比常见转述保守):NIST 会议评测 26 场会议中**按时长计约 12% 的前景发言时间被他人重叠**(各站点 7–16%),**按停顿分隔的语段计 30–50% 含重叠**;重叠区前后 ASR 错误率显著升高。来源:[Çetin & Shriberg, Interspeech 2006](https://www.isca-archive.org/interspeech_2006/cetin06_interspeech.pdf)

**2. 扬声器回放串入 mic 轨,必须 AEC;而 AEC 在双讲时结构性损伤近端语音。**
- 【官方】W3C 把该问题写进标准约束:echoCancellation "attempts to... reduce or eliminate crosstalk between the user's output device and their input device... negates the sound being produced on the speakers from being included in the input track"。来源:[W3C Media Capture and Streams](https://www.w3.org/TR/mediacapture-streams/) · [MDN 释义](https://developer.mozilla.org/en-US/docs/Web/API/MediaTrackSettings/echoCancellation)
- 【官方】WebRTC AEC3 源码类注释自认参考流与采集流对不齐是核心工程难题:"Partially handles the **jitter in the render and capture API call sequence**"。来源:[echo_canceller3.h](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/echo_canceller3.h)(本项目亲历同款问题:SCK 参考迟到破坏因果导致实时 AEC 一贯失效,PR#76 修复)
- 【论文】双讲(远端回放+近端人声同时)导致自适应滤波器发散,只能冻结自适应+残余抑制兜底——**双讲期近端语音被压制是 AEC 的结构性代价**,ASR 拿到的就是受损语音。来源:[Benesty et al., Signal Processing 2000](https://www.sciencedirect.com/science/article/abs/pii/S0165168400000499)
- 对照:VoIP 场景每个客户端只需消"自己扬声器→自己麦"的本地回声,且拿得到精确参考;单机双轨录音方是"第三方旁观者",参考信号要跨采集管线自己对齐。

**3. 系统轨是混音后单流,无逐人归属。**
服务端下行给客户端的是 N-1 路混音(Granola 官方:灰色气泡="system audio, usually **other people**"——复数,一轨多人)。一旦混成单声道,逐人归属只能靠声纹 diarization/多说话人建模重建,而这条链路错误率本身是量级级别(下条);单通道多说话人 SOTA(微软 SOT)也只到 AMI-SDM WER 21.2%。**对比 VoIP 服务端:同一时刻它明明持有 N 路带身份的独立流——混音丢弃的信息,事后要用错误率两位数的模型去猜。**
**4. 两轨时钟不同源:采样率漂移/时基对齐是物理事实,苹果官方成文承认。**
- 【官方】macOS 采系统音频的正路是 ScreenCaptureKit(`SCStreamConfiguration.capturesAudio`),与麦克风采集是两条独立管线、两个时钟域。来源:[capturesAudio](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/capturesaudio)
- 【官方】Apple 聚合设备文档:"**Drift correction**, also known as resampling, is used to **compensate for drift** in the data between devices that **aren't synchronized using hardware**... select the device with the most reliable clock as the clock source, then select the Drift Correction checkbox for each device."(对应 CoreAudio 键 `kAudioSubDevicePropertyDriftCompensation`)。来源:[Audio MIDI Setup 指南](https://support.apple.com/guide/audio-midi-setup/ams094c7edb4/mac)
- 【论文】CHiME-5 采集组同样被咬:"devices can drift out of synchrony due to small variations in clock speed (**clock drift**) and due to frame dropping",官方基线每 10 秒做一次互相关重对齐。来源:[arXiv:1803.10609](https://arxiv.org/abs/1803.10609) §4.1(本项目亲历:mic 轨采样率谎报致"说两遍且间隔越拉越大",27 场受影响)

**5. 说话人只能靠声纹 diarization 事后分,而 VoIP 逐流是"零成本正确"。**
- 声纹链路的错误率地板【官方项目基准】:pyannote 在真实会议 DER 13–23%(AMI-IHM 17.0–18.8%,AMI-SDM 19.9–22.7%,DIHARD-3 ~21.4%;闭源 precision-2 也要 12.9–15.6%);远场一致比近讲差 3–4pp。来源:[pyannote-audio README 基准](https://github.com/pyannote/pyannote-audio)
- 逐流方案的"错误率"仅来自信令事实(谁的流在响),业界产品口径与之吻合:Zoom RTMS 转写带 user_id/user_name【官方】、Teams 转写带 speaker's name【官方】、腾讯/飞书"同设备多人才需要拆"【官方】、Granola "Me vs Them + 平台补名"【官方】、通义听悟(纯声纹路线)官方自认"不能区别身份"【官方】。

## 五、对照表

| 产品 | 采集方式 | ASR 位置 | 说话人归属 | 来源等级 |
|---|---|---|---|---|
| Zoom(云录制/RTMS) | 逐参会者近场上行流(客户端 AEC+NS) | 云端(VTT) | **流归属**(user_id/显示名);无参会者声纹功能 | 官方(名字来源为强推断) |
| Microsoft Teams | 同上;媒体 bot unmixed 上限 4 路 top speakers | 云端 | **流归属**(账号名,可隐藏);会议室共享麦才用声纹(voice profile,需 Premium/Rooms Pro) | 官方 |
| 腾讯会议 | 逐参会者上行流 | 云端(天籁实验室;"智聆"无一手来源) | **流归属**;同设备多人→手动触发声纹拆分重生成 | 官方 |
| 飞书妙记 | 会议:逐参会者流;另收录音/上传文件 | 云端 | 个人设备=**流归属**;会议室设备/录音文件=自动声纹拆分("说话人 1/2") | 官方 |
| 钉钉闪记 | 会议录制/手机录音(单流) | 云端 | 声纹 diarization(营销页口径);引擎疑为通义听悟 | 官方弱+推断 |
| Otter.ai | bot 入会(访客身份)或 App 直接拾音 | 云端(AWS) | **声纹学习式**(tag 训练、跨会议认人) | 官方 |
| Plaud Note/NotePin | 硬件:双麦/四麦阵列 + VCS 振动传感器 | 云端(早期明示 Whisper) | 匿名 Speaker labels(diarization) | 官方 |
| 讯飞听见/录音笔 | 硬件:8 麦(SR501)/2+6 阵列(SR702,媒体) | 云端为主,录音笔可离线降级 | 角色分离标号,不认名 | 官方+媒体 |
| Granola | **单机双轨:mic + 系统回环**,无 bot | 云端第三方(Deepgram/AssemblyAI),不存音频 | **按轨二分 "Me vs Them"**;逐人认名靠平台集成补显示名 | 官方 |
| Krisp | 单机:虚拟麦/虚拟扬声器插入通话链路 | 端侧自研 ASR(官方宣称) | 我/对方按轨分;远端多人 diarization 标号 | 官方 |
| 通义听悟 | 网页/插件收音(纯云服务) | 云端 | 声纹分离,官方自认"不能区别身份",人工改名 | 官方 |
| 本项目 voice-notes | **单机双轨:mic(CoreAudio)+ 系统回环(SCK)** | **全本地** | 声纹 diarization + 跨会议种子匹配 + 人工整理 | — |

## 六、机制总结:为什么同样的 ASR 模型,VoIP 逐流远好于单机双轨

四层差距逐层叠加,每层都与模型无关:

1. **信号层**:VoIP 消费的是贴嘴近场、每人一路的干净流(客户端已 AEC/NS);单机双轨的 mic 轨是远场混响多人轨——仅此一层,同一模型 WER 就有近乎翻倍到 30+pp 的差距(AMI IHM 25.5%→SDM 49.3%;CHiME-5 47.9%→81.3%)。
2. **分离层**:VoIP 的"多说话人问题"在采集端物理解决(每人自带麦克风=天然完美分离);单机双轨要从混音里事后重建,重叠段(约 12% 时长、30–50% 话轮)天然受损。
3. **归属层**:VoIP 的说话人标签是信令事实(user_id→显示名),零成本、零错误率、直接出真名;单机双轨只能声纹 diarization(真实会议 DER 13–23%)且只出匿名标号,认名还要再过一道跨会话声纹识别或人工标注。业界所有纯声纹玩家(通义听悟、Notta、Plaud)都只承诺"说话人 1/2";所有能出真名的(Zoom/Teams/腾讯/飞书/Granola 补名/Tactiq)靠的全是平台元数据而非声学。
4. **工程层**:单机双轨独有两笔税——旁观者 AEC(参考需跨管线对齐,双讲期近端受损)与双轨时钟漂移(需重采样补偿);VoIP 客户端两者都在标准链路内解决。

**推论(供本项目决策)**:与 VoIP 逐流方案比"转写+归属准确率"没有胜算,这是物理与架构差距;单机双轨路线的正当性在别处——不入会(无 bot 礼仪/权限问题)、平台无关(任何 App 的声音都能录)、全本地(音频不出设备,对照:Granola 音频实时出境到 Deepgram/AssemblyAI、Otter 全云存储)、跨会议认人(Zoom/腾讯根本不做,Teams 要 Premium+会议室,Otter 做了但吃官司)。本项目的护城河应压在这四点上,而转写/归属侧的合理目标是"逼近可用",不是"追平 VoIP"。

## 参考来源汇总

**线上会议软件**【官方】:[Zoom SDK Raw Data API](https://marketplacefront.zoom.us/sdk/meeting/windows/class_i_zoom_s_d_k_audio_raw_data_delegate.html) · [Zoom RTMS](https://developers.zoom.us/docs/rtms/) / [media](https://developers.zoom.us/docs/rtms/meetings/media/) / [博客](https://developers.zoom.us/blog/realtime-media-streams/) · [Zoom 云录制转写 KB0064927](https://support.zoom.com/hc/en/article?id=zm_kb&sysparm_article=KB0064927) · [Zoom 专业音频 KB0059985](https://support.zoom.com/hc/en/article?id=zm_kb&sysparm_article=KB0059985) · [Teams 实时转写](https://support.microsoft.com/en-us/office/view-live-transcription-in-microsoft-teams-meetings-dc1a8f23-2e20-4684-885e-2152e06a4a8b) · [Teams real-time media](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/calls-and-meetings/real-time-media-concepts) · [AudioSocketSettings](https://microsoftgraph.github.io/microsoft-graph-comms-samples/docs/bot_media/Microsoft.Skype.Bots.Media.AudioSocketSettings.html) / [UnmixedAudioBuffer](https://microsoftgraph.github.io/microsoft-graph-comms-samples/docs/bot_media/Microsoft.Skype.Bots.Media.UnmixedAudioBuffer.html) · [Teams Rooms voice recognition](https://learn.microsoft.com/en-us/microsoftteams/rooms/voice-recognition) · [Teams 降噪](https://support.microsoft.com/en-us/office/reduce-background-noise-in-microsoft-teams-meetings-1a9c6819-137d-4b3b-a1c8-4ab20b234c0d) · 腾讯会议帮助 [1870](https://meeting.tencent.com/support/topic/1870/index.html) / [1868](https://meeting.tencent.com/support/topic/1868/index.html) / [1985](https://meeting.tencent.com/support/topic/1985/index.html) / [天籁新闻](https://meeting.tencent.com/news/bbsx241114.html) · 腾讯云 [53483](https://cloud.tencent.com/document/product/1095/53483) / [76740](https://cloud.tencent.cn/document/product/1095/76740) · 飞书 [重新识别说话人](https://www.feishu.cn/hc/zh-CN/articles/812241214493) / [生成妙记](https://www.feishu.cn/hc/zh-CN/articles/386045971891) · [钉钉闪记帮助](https://www.dingtalk.com/qidian/help-detail-1060922251.html)

**笔记/硬件产品**【官方】:Otter [OtterPilot](https://help.otter.ai/hc/en-us/articles/4425393298327-OtterPilot-Overview) / [Speaker ID](https://help.otter.ai/hc/en-us/articles/21665587209367-Speaker-Identification-Overview) / [Privacy](https://otter.ai/privacy-security) · Plaud [产品页](https://www.plaud.ai/products/plaud-note-ai-voice-recorder) / [VCS FAQ](https://support.plaud.ai/hc/en-us/articles/50837313885337-Can-I-use-headphones-earphones-when-I-record-a-phone-call-with-Plaud-Note) · [讯飞听见](https://www.iflyrec.com/) / [SR501](https://www.xunfei.cn/goods.php?id=33) · Granola [transcription](https://docs.granola.ai/help-center/taking-notes/transcription) / [security](https://www.granola.ai/security) · Krisp [blog](https://krisp.ai/blog/krisps-ai-meeting-assistant-transcription-notes/) / [原理](https://help.krisp.ai/hc/en-us/articles/4402174576402-How-Krisp-Microphone-and-Krisp-Speaker-work) · [Notta FAQ](https://support.notta.ai/hc/en-us/articles/4403163792027-Does-Notta-support-speaker-identification) · [Tactiq](https://tactiq.io/learn/how-to-live-transcribe-google-meet-calls) · 通义听悟 [概述](https://help.aliyun.com/zh/tingwu/what-is-tingwu) / [FAQ](https://help.aliyun.com/zh/tingwu/faq-about-basic-usage) / [动态](https://help.aliyun.com/zh/tingwu/release-notes)

**远场/基准/双轨难点**:【官方】[Shure MXA920](https://www.shure.com/en-US/products/microphones/mxa920) · [Poly](https://www.hp.com/us-en/poly/learning-hub/audio-innovations.html) · [Logitech RightSound](https://support.logi.com/hc/en-001/articles/360023206834-Understanding-RightSound-Technology) · [Kaldi AMI](https://github.com/kaldi-asr/kaldi/tree/master/egs/ami) · [CHiME-6 结果](https://chimechallenge.github.io/chime6/results.html) · [pyannote 基准](https://github.com/pyannote/pyannote-audio) · [W3C mediacapture](https://www.w3.org/TR/mediacapture-streams/) · [WebRTC AEC3](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/echo_canceller3.h) · [Apple capturesAudio](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/capturesaudio) · [Apple Drift Correction](https://support.apple.com/guide/audio-midi-setup/ams094c7edb4/mac) 【论文】[CHiME-5 arXiv:1803.10609](https://arxiv.org/abs/1803.10609) · [CHiME-6 arXiv:2004.09249](https://arxiv.org/abs/2004.09249) · [CHiME-7 arXiv:2306.13734](https://arxiv.org/abs/2306.13734) · [Kanda 2021 arXiv:2103.16776](https://arxiv.org/abs/2103.16776) · [Çetin & Shriberg 2006](https://www.isca-archive.org/interspeech_2006/cetin06_interspeech.pdf) · [Benesty DTD 2000](https://www.sciencedirect.com/science/article/abs/pii/S0165168400000499) 【媒体】[极客公园 SR702](https://www.geekpark.net/news/316001) · [Zoom Community 归属讨论](https://community.zoom.com/meetings-2/meeting-recordings-captions-speaker-attribution-12333)
