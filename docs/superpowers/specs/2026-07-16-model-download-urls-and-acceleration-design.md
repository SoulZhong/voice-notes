# 模型下载:地址显示 + 国内多代理加速 + 并发下载

日期:2026-07-16
状态:设计已确认,待写实现计划

## 背景与目标

设置页「语音模型」区管理 8 个模型工件的下载。本次针对用户提出的三个诉求 + 一个线上 bug:

1. **显示下载地址**:每个模型能看到它从哪下(原始源 + 镜像生效地址)。
2. **国内快速下载**:优化现有「镜像加速」,让国内用户(尤其大文件)更稳更快。
3. **并发下载**:多个模型同时下,而非当前的串行。
4. **[已修复] DTLN 下不了**:见下方「已完成的带外修复」。

### 明确不做(边界)

- 不接 ModelScope。核实结论:ModelScope 上大文件(SenseVoice 1GB / Whisper / Paraformer)没有与 GitHub 相同的 `.tar.bz2` 打包,只有第三方个人号上传的解包散文件,直接换会破坏「下 tar.bz2 → 解压」加载逻辑;能对上的只有 3 个最小 onnx,价值低且源不可控。故放弃。
- 不改模型加载 / 解压逻辑,不改 sha256 校验链。
- 不给镜像前缀加 UI 编辑框(用户明确选择「只改默认值」);健壮性改由「多代理自动回退」补足。
- DTLN 继续自托管于 `SoulZhong/voice-notes` release —— 全网无官方 onnx 源(上游 breizhn/DTLN-aec 只发 tflite),已核实。

## 已完成的带外修复:DTLN release 缺失

**根因**:代码里 DTLN 两个工件指向 `github.com/SoulZhong/voice-notes/releases/download/models-dtln-aec-v1/*.onnx`,但该 release **从未发布**(owner 登录态 `gh release view` 报 `release not found`,release 列表也无此项)。仓库本身是 public,但目标 release 不存在 → 除开发者本机已有文件外,**所有用户下载必然 404**。

**修复(已执行)**:用 `gh` 创建公开 release `models-dtln-aec-v1` 并上传本机 `src-tauri/models/dtln_aec_256_{1,2}.onnx`(字节/sha256 与代码 `ARTIFACTS` 登记值完全一致)。已匿名实测两资产 HTTP 200、字节与 sha256 均正确,经 ghfast.top 亦可达。**现有代码 URL 立即对全体用户生效,无需改代码。**

**遗留注意**:今后若更新 DTLN 模型或换 tag,必须同步发布对应 public release,否则复现此 bug。建议在模型工件注册处留注释提示。

## 模块一:设置页显示每个模型下载地址

### 现状障碍

前端 DTO `ArtifactState`(`src/lib/models.ts`)不含 URL 字段,`models_status` 命令也不返回 URL。

### 设计

- **后端**:给 `models_status` 返回的 `ArtifactState`(定义处含 id/label/approx_mb/required_for_recording/present)新增 `url: String`,取自 `ARTIFACTS[i].url`。纯序列化多带一个已有值,零逻辑改动。对应更新 `src/lib/models.ts` 的 `ArtifactState` 类型加 `url: string`。
- **前端**(`src/routes/settings/+page.svelte` 模型行,约 760–802):
  - 每行加展开箭头,手风琴式(同时只展开一个 `expandedId`,单选状态)。展开触发区限定在行标签区域,避免与「下载/删除」按钮冲突。
  - 展开面板显示:
    - **原始地址**:`a.url` 全文 + 复制按钮。
    - **镜像地址(生效)**:镜像开启时 = `前缀 + a.url`(前端小工具函数复刻后端 `apply_mirror` 逻辑:前缀非空则拼接,自动补尾 `/`);镜像关闭时显示「未启用镜像加速」。
  - DTLN 两行如实显示 `SoulZhong/voice-notes` 链接(既定事实)。
- 样式跟随现有 `.row` / `.row-desc` 体系,展开面板用等宽小字 + `overflow` 处理长 URL,复制按钮复用现有 `.link` / `.btn-secondary` 样式。

## 模块二:国内加速 —— 多代理自动回退

### 现状

`download::download_urls(url, enabled, prefix)` 已返回「按序尝试的 URL 列表」,当前为 `[前缀+url, 原站url]`(见 `download.rs`)。`mirror_prefix` 默认 `https://ghproxy.net/`,持久化在 `settings.json`,UI 无编辑入口。

### 设计

1. **默认前缀改为 ghfast.top**:`DEFAULT_MIRROR_PREFIX = "https://ghfast.top/"`(存活实测健在、CDN 支撑、社区口碑好)。
2. **一次性迁移存量旧默认**(`settings.rs`):新增 `LEGACY_MIRROR_PREFIX = "https://ghproxy.net/"`;应用启动时执行一次 `settings::update`——若持久化的 `mirror_prefix == LEGACY_MIRROR_PREFIX` 则抬到新默认。因 UI 从不允许编辑,存量值只可能是旧默认,迁移安全、不会误改用户自定义值。新老用户均生效。迁移与现有 `settings::update`(带 WRITE_LOCK 串行化)复用同一路径。
3. **多代理自动回退**:`download_urls` 扩为
   `[主代理+url, 备用代理1+url, 备用代理2+url, …, 原站url]`。
   - 主代理 = 用户当前 `mirror_prefix`(默认 ghfast.top)。
   - 备用代理 = 一小组硬编码存活代理常量(如 `gh-proxy.com`、`ghproxy.net`);去重(与主代理相同则跳过),列表短(总代理数 ≤ 3)。
   - 镜像关闭时退化为 `[原站url]`,与现状一致。
   - 「测试」按钮(`probe_mirror`)仍只测主前缀,行为不变。
4. **死代理快速跳过**:现有下载循环对每个 URL 会重试 `DOWNLOAD_ATTEMPTS_PER_URL` 次;死代理的连接超时属「可重试」错误,会在单个死代理上空耗多次重试再换下一个。为避免多代理放大延迟,实现计划需处理:对回退候选采用「每代理少重试 / 连接类失败快速换下一个」,把整轮尝试的最坏延迟压住(原站作为最终兜底可保留正常重试次数)。具体策略在计划中定。

## 模块三:并发下载

### 现状

`download_models` 命令(`src-tauri/src/lib.rs` 约 2160–2229)对选中工件**串行** `for a in selected` 逐个下载,且**任一工件失败即 `break` 整个循环**——后续工件不再尝试。

### 设计

- 将串行循环改为**有并发上限的并行下载**(建议并发 2–3;大文件占带宽,不宜过高)。可用固定大小的工作线程池消费工件队列(`download_artifact` 为同步阻塞 IO)。
- **进度**:每个工件独立发 `model_download` 事件(现有事件已按 `artifact` id 区分,前端 `prog` map 已按 id 分槽,天然支持并发多进度条,前端无需改结构)。
- **失败隔离**:一个工件失败不再中断其余(顺带修掉现状的连带中断)。各工件独立走模块二的 URL 回退链;失败各自 emit `error` 事件。
- **会话锁与取消**:保留单下载会话 `guard`(一次只有一个下载会话,会话内并行多工件);`cancel` AtomicBool 由所有 worker 共享,取消对所有在途工件生效。
- **汇总**:所有 worker join 后聚合 `all_ok`(全成功才发「全部完成」),再 `drop(guard)`;保持「收到 done 即可再次下载」的时序。
- **前端**:模型区已有「下载」单行按钮;如存在「下载全部/按选型下载」入口,其触发的多工件下载即自动享受并发。并发上限、按钮禁用态沿用现有 `downloadingActive` 判定。

## 涉及文件一览

- `src-tauri/src/models/mod.rs` —— `ArtifactState` DTO 加 `url`;DTLN 注册处补「需同步发布 release」注释。
- `src-tauri/src/models/download.rs` —— `download_urls` 多代理回退;备用代理常量;死代理快速跳过策略。
- `src-tauri/src/settings.rs` —— 默认前缀改 ghfast.top;`LEGACY_MIRROR_PREFIX`;启动一次性迁移。
- `src-tauri/src/lib.rs` —— `models_status` 填 `url`;`download_models` 串行改并发 + 失败隔离 + 启动迁移调用点。
- `src/lib/models.ts` —— `ArtifactState` 加 `url`。
- `src/routes/settings/+page.svelte` —— 模型行展开显示地址;镜像地址前端拼接工具函数。

## 风险与已知代价

- **境外测不出国内真实速度**:新默认基于「存活 + 口碑」而非实测国内最快;有「测试」按钮兜可达性。
- **无 UI 编辑的固有脆弱性**:靠多代理回退缓解(某代理挂了自动试下一个 / 最终回退原站);若全部代理与原站在某地区皆不可达,仍需发版调整代理列表。
- **多代理延迟放大**:靠模块二第 4 点「死代理快速跳过」控制。
- **并发带宽争用**:并发上限 2–3 折中;不做用户可配。
- **DTLN 维护约束**:模型更新须同步发布 public release(见带外修复遗留注意)。

## 测试

- **settings.rs**:沿用现有单测风格,补「迁移:存量 `ghproxy.net` → 新默认」「自定义/新默认值不被迁移误改」用例。
- **download.rs**:`download_urls` 多代理回退顺序(镜像开/关、主备去重)纯函数单测,复用现有 `apply_mirror` / `download_urls` 测试风格。
- **并发下载**:网络路径按仓库惯例靠人工冒烟(现有 `download_artifact` 即不做网络单测);可为「队列消费 + 失败隔离 + 取消传播」的可纯化部分补单测(若结构允许)。
- **人工冒烟**:全新安装(空 settings)→ 默认 ghfast.top;存量 `ghproxy.net` → 迁移生效;断开主代理验证回退;多模型并发下载各自进度 + 单个失败不拖累其余;设置页展开显示原始/镜像地址正确。
