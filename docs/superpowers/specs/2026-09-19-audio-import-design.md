# 音频导入:把本地录音文件变成一篇笔记

日期:2026-09-19 · 状态:已实现

## 要解决什么

手上已经有一个录音文件(手机录的、Teams/腾讯会议导出的、别人发来的),想让它进
voice-notes 走一遍完整的会议笔记流程:转写、分人、Aing 精修、拟题、进知识图谱。
在此之前,唯一的入口是「现在开始录」——历史录音只能干瞪眼。

## 一句话设计

**导入 = 建一篇只有 mic 轨的笔记,再对它跑一遍离线转写。**

后半句原样复用重转写链路(`run_retranscribe_once`),不另起管线。导入侧新增的代码
只负责前半句:把任意格式的音频解码成本仓标准轨(16k / 单声道 / s16le / 标准 44 头),
分配笔记目录,写 meta。

```
选文件(前端 plugin-dialog)
  → import_audio 命令(async,丢阻塞线程池)
     ├ 守卫链(照抄 do_retranscribe 的顺序纪律)
     ├ 占 retranscribing 槽(note_id 先留空串)
     ├ import::create_note:解码 → 分配目录 → 落 mic.wav → 写 meta(state=complete)
     ├ 槽改写成真 note_id
     └ 返回 note_id(前端 bumpNotes + goto 新笔记页)
  → spawn_retranscribe(mixed=false, engine=None, refine_after=true)  [后台线程]
     ├ retranscribe::run:切段 → ASR → 段内切分 → 声纹归属 → 提交
     ├ 清槽
     └ spawn_refine(.., enqueue_transcode=true)
        └ 转码移交 / 日历匹配 / LLM 精修 / identify / 拟题 —— 与停录后逐字相同
```

## 几个定死的选择

### 为什么轨名是 `mic` 而不是 `mixed`

直觉上导入文件更像"成品轨"(一条轨里所有人都在),但 `mixed` 在本仓有专门语义——
它是 mic+system **混出来的派生物**,因此:

1. `store::audio::list_tracks` 刻意把 mixed 排除在播放器轨列表之外(源轨与成品轨
   叠播音量翻倍)。导入笔记若只有 mixed 轨,笔记页将**一条可播的轨都没有**。
2. `retranscribe::input::mixed_untrusted` 要拿源轨读数对账成品轨完整性。导入笔记
   没有源轨可对,恒判"不可信",成品轨重转写入口永久置灰。

而"只有 mic、没有 system"本就是合法笔记形态(只录麦克风的场次):播放、波形懒回填、
转码、`DualTrackInput` 重转写(轨不存在即 continue)、裁剪导出,全链路都已支持。
导入直接落在这条既有形态上,零新增分支。

代价:播放器那条轨在双轨笔记里会被标成「麦克风」。实测无碍——音轨菜单是
`{#if tracks.length > 1}` 才渲染,单轨笔记根本不显示这个菜单。笔记页头部另有
「导入」标(hover 出源文件名)说明出身。

### 为什么复用 `retranscribing` 槽而不是新开一把

导入的后半程就是一次离线转写(同一套 VAD/ASR/声纹 ORT 管线),它与录制、重转写、
补生成互斥的理由**逐条相同**。新开一把槽意味着要在 `spawn_session`、
`do_regenerate_mixed`、`spawn_refine` 三处再接一轮 Dekker 写后读并各自重做一遍
互斥证明——那是本仓最容易接错的地方(见那几处的 Fix 1A/1B/2 注释)。复用现槽,
这些互斥关系一条不落地自动成立。

附带好处:笔记页照既有逻辑显示"这篇正在分析中"并在完成时自刷新,前端零新增事件、
零新增轮询命令。

代价:`retranscribe_last` 会记下导入任务的终态,MCP/UDS 的 `retranscribe_status`
轮询方会看到它——那同样是一次转写的终态,不算失真。

### 中间态是刻意的

`create_note` 返回后,盘上是一篇**有音频、没转写**的笔记(state=complete、
segments.jsonl 不存在)。转写随后在后台跑。

这么设计是为了转写失败时**不把音频一起丢掉**:用户仍能播放、能换引擎点「重新分析」。
反过来,把笔记删掉才是更糟的失败模式——用户刚导进来的东西凭空消失。

崩溃残局同理:一篇 0 段的 complete 笔记,点一下「重新分析」就能自愈。

### started_at 取导入时刻,不取文件 mtime

文件的修改时间常年被复制/转存抹平,一个 2020 年的 mtime 会把新导入的笔记直接沉到
列表最底下,用户会以为导入失败了。`ended_at` 顺延音轨时长,于是列表副标题的时长
读数在转写落段之前就已经是对的(`summarize` 的 `duration_from_meta` 回退)。

### 标题:文件名 vs 让 Aing 拟题

`store::writer::is_default_title` 是 Aing 自动拟题(`rename_if_default`)的唯一闸门,
标题一旦不是默认样式,自动拟题整篇让路。所以把「20260918_143022」写进标题,等于用
一串没有信息的数字**永久**占掉那个位置。

判据(`import::title_from_filename`):折叠空白后把数字与常见分隔符从首尾剥掉,剩下的
若为空、或整体落在一张"设备自动生成名"表里(录音 / New Recording / voice / untitled…),
就用默认标题、把拟题让给 Aing;反之(「客户访谈-张三」)是人自己起的名,比 LLM 拟的题
更权威,直接采用(按字符截到 60)。

误判两边的代价不对称:误判成"人起的名"会让一串数字永久占住标题;误判成"自动名"
只是多一次 LLM 拟题。所以表里宁可多收几个常见词。

## 平台

| | 格式 | 实现 |
|---|---|---|
| macOS | mp3 / m4a / m4b / aac / wav / aif(f) / caf / mp4 / flac | 系统内建 `afconvert -f WAVE -d LEI16@16000 -c 1`(与本仓其余音频转换同一条路) |
| 其余 | 仅 wav | 纯 Rust:hound 解码 + 声道平均下混 + `resample_linear` 降到 16k |

`resample_linear` 就是实时采集链路 48k→16k 用的那一条,导入不另立口径。
非 macOS 的格式面缺口与本仓既有的平台落差一致(转码、离线回声清洗同样只在 macOS),
后续要补的话是引入 symphonia 这类纯 Rust 解码器。

macOS 上 afconvert 解 WAV 失败时(非标准块序、奇异位深)会降级到纯 Rust 路径——
一条我们自己就能读的轨,不该因为子进程挑食而导入失败。

## 失败语义

- **解码失败 / 时长为 0**:笔记目录根本不建,中转件删掉,命令同步报错。盘上零残留。
- **建档中途失败**(rename/写 meta):整个笔记目录删掉。
- **转写失败**:笔记与音频留住,错误经 `retranscribe` 事件报到笔记页。不发起 Aing。
- **崩在解码中途**:notes 根下留一个 `.import-*.wav` 中转件(不是目录,`NoteStore::list`
  不枚举它)。下次导入入口会顺手清掉。

## 新增/改动面

后端:
- `src-tauri/src/import.rs`(新):格式白名单、标题判据、解码器、建档。8 个单测。
- `lib.rs`:`do_import_audio` 守卫链 + `import_audio` 命令;`spawn_retranscribe` 加
  `refine_after` 形参(手动重转写 false、导入 true)。
- `store/writer.rs`:抽出 `alloc_note_dir`(录制建档与导入建档共用同一份 id 规则)。
- `store/mod.rs`:`NoteMeta.imported_from`(serde default,兼容旧 meta)。
- `store/transcode.rs`:`decode_m4a_to_standard_wav` → `decode_to_standard_wav`(正名,
  实现一字未动:解码参数与输入容器无关)。
- `mcp/uds.rs` + `mcp/server.rs`:`import` op / `import_audio` 工具,过
  「允许 AI 控制录制」同一道门。

前端:
- `Sidebar.svelte`:录制药丸下方的次级入口「导入音频」(带文字,不用纯图标——同样的
  幽灵图标钮在重转写入口上被实测"没人找到")。
- `notes.ts`:`importAudio` / `IMPORT_EXTS`。
- 笔记页:`imported_from` 亮「导入」标,hover 出源文件名与"单轨/无回声消除"的口径说明。
- i18n:`shell.import.*` / `notes.imported.*`(zh + en)。

## 已知限制 / 后续

1. 非 macOS 只能导 WAV(见上表)。
2. 一次一个文件。批量导入要在后端排队(单槽拒绝的语义下,并发导入会被拒而不是排队)。
3. 导入笔记没有 `sync` 对账记录,因此漂移诊断、`mixed_untrusted` 这类要源轨读数的
   功能对它天然不适用——这是如实的"无数据",不是缺陷。
4. 长文件内存:非 macOS 的纯 Rust 路径会把整条 PCM 读进内存。与 `track_pcm` 的既有
   口径一致(重转写本来就整轨读),但超长文件在低内存机器上需留意。
5. 超过约 37 小时的音频会被拒(WAV 的 data 长度字段是 u32)。
