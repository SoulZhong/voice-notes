# P5 backlog 硬化(6 项终审 defer 项)设计

日期:2026-07-04
来源:P5 全分支终审判 defer 的后续项(.superpowers/sdd/progress.md P5 节),用户决定全部 6 项一次做完。
分支:`p5-backlog-hardening`,单 PR squash 合入 master。

## 范围

| # | 项 | 类型 |
|---|---|---|
| 1 | speakers.json / segments.jsonl 非活动写者并发丢更新 | 后端稳定性 |
| 2 | export 与详情页 filter+sort 语义不一致 | 一致性 |
| 3 | download_running 无 drop-guard,线程 panic 卡死下载 | 后端稳定性 |
| 4 | 录制中下载完成触发 preload 双载,瞬时 2x 内存且白载 | 后端稳定性 |
| 5 | tar.bz2 解压阶段取消不即时 | UX |
| 6 | 模型下载大卡片↔小提示条切换重建组件,UI 态清零 | UX |

明确不做:阈值校准(继续挂起,数据驱动);reject_if_active 检查与写盘间的理论竞态(与 rename_note 同模式,沿既有接受);孤儿说话人清理。

## 1. 非活动写者全局写锁

**现状**:非活动笔记编辑走 `NoteStore`(无状态,每命令 `new(dir)`),read-modify-write 无互斥。并发的 `rename_speaker`(非活动分支,lib.rs:666)与 `set_segment_speaker`(lib.rs:712)各自读旧表、改内存、整表原子写,后落盘者覆盖前者。segments.jsonl 的三个编辑原语(edit_segment_text / delete_segment / set_segment_speaker)之间同理。活动写者经 `Arc<Mutex<NoteWriter>>` 串行,不受影响。

**设计**:锁内建于 `NoteStore`——`store/notes.rs` 增模块级 `static EDIT_LOCK: Mutex<()>`,全部变更方法(`rename` / `delete` / `rename_speaker` / `edit_segment_text` / `delete_segment` / `set_segment_speaker`)入口持锁。调用方(lib.rs commands)零改动、无法遗忘;store 层可直接单测。锁毒化用 `into_inner` 忽略(单次写各自原子,毒化无残留)。

跨笔记串行:这些都是毫秒级、单用户 UI 驱动的稀有操作,无感知代价;不做 per-note map(粒度收益为零,徒增管理)。锁只罩非活动路径,与活动写者的 `NoteWriter` 锁互不相干(非活动路径已由 `reject_if_active` 与活动写者隔离)。

**测试**:两线程并发 rename_speaker + set_segment_speaker("new") 各 N 轮,终态两者的改动都在(无丢更新)。

## 2. filter+sort 下沉 NoteStore::load(单一真值源)

**现状**:详情页 `displaySegments`(notes/[id]/+page.svelte:38-45)做 `.filter(text.trim())` + `.sort(start_ms, seq)`;export(store/export.rs)按 segments.jsonl 追加顺序原样输出——导出含空白段、保留 ECHO hold 乱序,与页面所见不一致。

**设计**:`NoteStore::load` 末尾对 `segments` 统一施加:
1. 过滤空白段(`!text.trim().is_empty()`;不计入 skipped_lines——非损坏)
2. 稳定排序 `sort_by(start_ms asc, seq asc)`

export 与 getNote(详情页/录制页水合)自动继承。前端 `displaySegments` 的 filter+sort 随之删除(单一真值源,防两处语义漂移)。

**边界确认**:编辑原语走独立的 `read_jsonl_lines`(原始行),不经 load——磁盘文件序、损坏行原样保留、seq 定位均不受影响。`max_end_ms`(时长)直读文件,不变。

**测试**:load 对乱序+空白段 fixture 返回过滤排序后的段;export 输出与 load 顺序一致、无空白段;现有编辑测试不回归。

## 3. download_running RAII drop-guard

**现状**:清位是下载线程闭包尾部一条普通 `store(false)`(lib.rs:813),置位(771)到清位间任意 panic 都会让标志永久卡 true,此后 `download_models` 一直报"下载已在进行中"。

**设计**:线程闭包顶部构造 RAII guard(持 `Arc<AtomicBool>`,`Drop` 里 `store(false, SeqCst)`),删除手动清位。panic 展开也走 Drop,标志必然释放。guard 在 `emit("all","done")` 与 preload 触发之前显式 `drop`,保持"done 事件到达时可再下载"的现状时序。

**测试**:模拟线程内 panic(测试钩子或抽出可注入闭包),断言标志复位、可再次发起下载。

## 4. preload 会话活跃跳过 + 停录补预载

**现状**:开录 `take()` 走常驻识别器(槽 None)。录制中下载完成 → `preload_models`(lib.rs:817)见 `is_none()` 成立,再载一份 SenseVoice(数百 MB)与会话手中那份共存(瞬时 2x);停止时 `stash_model` 无条件回灌,把预载那份顶掉丢弃——白载。

**设计**:
1. `preload_models` 增参会话引用(`Arc<Mutex<Option<...>>>`),线程内先查 `session.lock().is_some()` → 活跃则整体跳过(打日志)。
2. `stop_recording` 收尾(stash 之后)补调一次 `preload_models`(幂等:槽有货跳过)——覆盖"录制中补下声纹→停止→再开录 diarization=on"衔接,不留空槽等下次开录现场加载。

调用点三处对齐:setup 启动预载、下载完成预载、停录补载。

**测试**:会话活跃时调 preload → 槽保持 None(未加载);停录后槽被回灌/补载。

## 5. 解压阶段即时取消(CancelReader)

**现状**:`cancel` 只在下载读循环检查(download.rs:152-156);`extract_and_install`(download.rs:65-101)的 `tar::Archive::unpack`(75)是同步一次性调用,解压期间取消无响应。

**设计**:`extract_and_install` 增 `cancel: &AtomicBool` 参;新增 `CancelReader<R: Read>`(包装底层 File,每次 `read()` 先查标志,置位则返回 `io::Error`;ErrorKind 用 `Other` 而非 `Interrupted`——后者会被 Read 消费者自动重试,永远断不掉),套在 BzDecoder 之下——字节级响应,大文件中途即断。unpack 出错后按 cancel 标志归一为 `bail!("cancelled")`,前端收到既有 `cancelled` phase,无协议变化。解压 tmp 目录即时清理 + 既有 `sweep_tmp` 兜底。

**连带修复(取消后的续传)**:取消解压时 `.part` 已是全量 tarball,原逻辑重试会发 Range 全量偏移 → 416 → 删分片 → 整包重下。把下载成功后的收尾(校验/解压安装/清 .part)抽成 `finalize_artifact`,416 分支直接调它:全量完好的 tarball 免重下、原地复装;脏残留则校验失败 → 删分片报错(同旧行为)。取消的解压保留 `.part`(下载失败的脏数据才删)。

**测试**:cancel 置位后 `extract_and_install` 返回 "cancelled"、无半装产物、tarball 保留;`finalize_artifact` 对全量有效 .part 免网络完成安装。

## 6. 下载卡片单实例化(compact 变响应式 prop)

**现状**:record/+page.svelte:43-47 大卡片与小提示条分属 `{#if !recording_ready}` / `{:else if !diarization_ready}` 两分支,各自实例化 `ModelDownloadCard`。识别模型下完、仅剩声纹时切分支 → 组件销毁重建,`downloading/prog/error/cancelled` 全清零(后端下载仍在跑,UI 进度丢失)。

**设计**:外层合并为单分支 `{#if models && !(recording_ready && diarization_ready)}`,单实例 `<ModelDownloadCard status={models} compact={models.recording_ready} ...>`。组件自身零改动——`compact` 已是 `$props()` 响应式 prop,且仅在模板层消费(class 与两处条件渲染),onMount 订阅与 compact 无关,状态天然跨切换保留。

**测试**:npm check 0/0;人工冒烟——录制模型下载中途 recording_ready 翻转,进度条不清零。

## 验收

- cargo test 全过(新增各项单测);npm check 0 errors 0 warnings;build OK。
- 人工冒烟(合并前):① 详情页并发改名/改说话人不丢 ② 导出与页面一致 ③ 下载中取消解压即断 ④ 录制中下载完成不涨 2x 内存、停录后再开录 diarization 正常 ⑤ 大小卡片切换进度不清零。
