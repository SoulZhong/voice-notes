//! 全部送给 LLM / Agent 执行体的提示词,集中在此一处,便于整体 review 与比对。
//!
//! 约定:
//! - 本文件**只放提示词文本与其拼装**,不放任何业务逻辑、网络调用或解析;
//!   调用方(llm.rs / agent.rs / identify.rs)只负责把返回值塞进请求体。
//! - 提示词一律用**原始字符串** `r#".."#` 且顶格书写。Rust 的原始字符串会原样保留
//!   缩进,顶格是为了让提示词的真实换行/行首与源码所见一致——读到什么就是发出去什么。
//! - 需要插值的用 `format!`(而非运行时 replace),这样占位符名写错在编译期就报错;
//!   原始字符串里字面量花括号仍需写成 `{{` / `}}`。
//! - 改动这里的任何一个字都等于改动线上行为。实际发出的完整提示词(含运行时拼接的
//!   上下文)记录在 `<data_dir>/ai_logs/*.json` 的 request 字段,可事后核对。

// ─────────────────────────────────────────────────────────────────────────────
// 一、HTTP LLM(OpenAI 兼容接口)的 system 提示词 —— 消费方 refine/llm.rs
// ─────────────────────────────────────────────────────────────────────────────

/// 逐字稿精修主提示词:四类修订 + 实体/关系抽取,一次调用同时产出。
/// 与 [`agent_refine`] 是同一套四类修订规则的两种投递方式(HTTP 直接给全文,
/// Agent 走 MCP 工具读写),改其一时务必同步看另一处。
///
/// **配套 user 消息**见 [`refine_user`]——**说话人姓名就是从那里进模型的**
/// (每段前的 `speaker=` 标注,由 [`refine_paragraph_line`] 排版)。本段只规定
/// 怎么用它:限于人名/称呼错字判断与实体归一,不得据此改写句式或写进正文。
/// 注意精修**拿不到声纹库全量名单**,只有本块各段自己的 speaker 标注;
/// 需要全库候选人的是身份推断,见 [`IDENTIFY_SYSTEM`]。
pub const REFINE_SYSTEM: &str = r#"你是会议逐字稿精修助手。对输入的每个段落做四件事,除此之外禁止任何改动:
1. 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文,禁止改写句式或语义;
2. 实体归一:同一人名/产品名/术语全文统一为最常见或术语表给定的写法;
3. 轻度清理口头语:删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留语气词「吧」「啊」等;
4. 英文与数字排版:英文词组与中文之间加空格,产品名保持原大小写。
此外,抽取本批出现的关键实体(不改动正文),用修订后的规范名,并抽取有原文证据的语义关系。关系 predicate.type 只能是 participates_in、responsible_for、belongs_to、uses、depends_on、produces、assigned_to、occurs_at,或 custom;custom 必须提供非空 label。每条关系给出 0 到 1 的 confidence。valid_from/valid_to 可为 null；非 null 时必须是带时区的 RFC3339 时间戳，且两者同时存在时 valid_from 必须严格早于 valid_to（不允许零长度区间）。evidence.paragraph_index 必须使用输入中标注的全文绝对段落下标,绝不能改成块内下标;start/end 是该修订后段落的 Unicode scalar(char)半开区间,不是 UTF-8 字节偏移;quote 必须逐字符精确等于该区间。
每段前的 speaker= 标注是该段说话人(人名或簇号),仅供理解上下文:用于人名/称呼错字判断与实体归一(如称呼「小王」后由 speaker=王某 的段应答,可确认「王」字写法)。禁止据此改写句式、把代词替换成人名、或把 speaker 标注/说话人名写进 texts;texts 只输出修订后的正文。
输出 JSON:{"glossary":{"错误写法":"统一写法"},"texts":["段落1修订文","段落2修订文"],"entities":[{"name":"规范名","kind":"person|org|project|term|decision|task|place|date","aliases":["别名"]}],"relations":[{"subject":"张三","predicate":{"type":"responsible_for","label":null},"object":"灯塔计划","confidence":0.92,"valid_from":null,"valid_to":null,"evidence":[{"paragraph_index":0,"start":0,"end":8,"quote":"张三负责灯塔计划"}]}]}。
texts 数组长度必须与输入段落数一致,顺序一致。glossary 只收实体类归一项。entities 没有可给空数组,aliases 可省略。relations 必须存在,没有关系时给显式空数组。"#;

/// 只补关系(正文与实体已定稿)。用于精修已完成、仅图谱需要重建的回填路径。
pub const RELATION_ONLY_SYSTEM: &str = r#"你是会议语义关系抽取器。正文和实体已经定稿，禁止改写、补写或删除任何段落与实体。只根据给定 paragraphs 和 entities 抽取有逐字证据的 relations。subject/object 必须使用 entities 中的规范 name 或 alias；predicate.type 只能是 participates_in、responsible_for、belongs_to、uses、depends_on、produces、assigned_to、occurs_at 或 custom，custom 必须带非空 label；confidence 必须在 0 到 1；valid_from/valid_to 为 null 或带时区 RFC3339，且 from 严格早于 to。evidence.paragraph_index 是全文绝对下标；start/end 是 Unicode scalar 半开区间；quote 必须逐字符等于该区间。只输出 JSON 对象 {"relations":[...]}；没有可靠关系时输出 {"relations":[]}。"#;

/// 起笔记标题(HTTP 路径)。正文经 user 消息给出,此处只给约束。
/// Agent 路径的同名任务见 [`agent_title`](fn.agent_title.html)——那边没有 system 位,
/// 约束与正文拼在同一条 prompt 里,措辞因此略有不同。
pub const TITLE_SYSTEM: &str = r#"你为会议转写起标题。只输出一个不超过 12 个字的中文标题,概括这场对话的核心主题;不要引号、标点或任何解释。"#;

/// 「测试连接」探测(HTTP 路径):最小请求,只验端点可达 + 鉴权通过 + 模型可用。
/// Agent 路径的对应物是 [`AGENT_PROBE`]。
pub const LLM_PROBE_USER: &str = r#"回复 OK"#;

/// 精修一块的 user 消息。system 侧是 [`REFINE_SYSTEM`]。
///
/// - `glossary`:上一块产出的术语表(JSON),跨块前传以保证全文实体写法一致;
///   首块为空对象。模型可沿用也可扩充,回包里带回新的一份。
/// - `numbered`:本块段落,逐行由 [`refine_paragraph_line`] 排版。
pub fn refine_user(glossary: &serde_json::Value, numbered: &str) -> String {
    format!("术语表(沿用并可扩充):{glossary}\n段落:\n{numbered}")
}

/// 精修输入里的一行段落。**`speaker` 是说话人姓名进入模型的唯一通道**:
/// 取值优先用人名(已命名/已关联的会议搭子),没有名字才退回 `R` 簇号,
/// 规则见 `llm::speaker_label`。`index` 是全文绝对段落下标——`REFINE_SYSTEM`
/// 要求 evidence 回引这个下标,块内下标会导致证据错位。
pub fn refine_paragraph_line(index: usize, speaker: &str, text: &str) -> String {
    format!("paragraph_index={index} speaker={speaker}: {text}\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// 二、说话人身份推断 —— 消费方 refine/identify.rs(HTTP)与 refine/agent.rs(Agent)
// ─────────────────────────────────────────────────────────────────────────────

/// identify 的 system 提示词。与 [`REFINE_SYSTEM`] 同风格:单行、契约逐条钉死。
/// HTTP 与 Agent 两条路径共用同一份文本,差异只在外层包装(见 [`agent_identify`])。
///
/// **这段就是「从上下文推断说话人」的全部规则**,四类证据写在正文里:
/// `self_intro`(自我介绍)、`addressed_reply`(称呼—应答配对,须给两条证据)、
/// `third_person_exclusion`(第三人称排除)、`role_topic`(角色/主题弱线索)。
///
/// **配套 user 消息不是文本模板,而是 `IdentifyContext` 的 JSON**
/// (`serde_json::to_string(ctx)`,构造见 `identify::build_context`)。四个字段:
/// - `clusters`:本场说话人簇(簇号/时长/是否 mic/已确认身份);
/// - `candidates`:**已有的会议搭子名单**——先取日历参会人 ∩ 声纹库
///   (`identify::calendar_candidates`),再并入声学近邻 top-k ∪ 最近共同出现
///   (`identify::recall_candidates`),去重后截到 `MAX_CANDIDATES`。
///   本段正文只规定怎么用它(二选一:`candidates` 里的 `person_id`,或候选外的
///   `new_name`),**名单本身是运行时数据,不在本文件**;
/// - `sampled`:按预算采样的会议片段(`identify::sample_paragraphs`);
/// - `calendar`:当场日历事件(可选),参会人上限 [`super::identify::CALENDAR_ATTENDEES_IN_PROMPT`]。
pub const IDENTIFY_SYSTEM: &str = r#"你是会议说话人身份推断器。输入 JSON 含三部分:clusters(说话人簇:speaker 是簇号,total_ms 时长,is_mic=true 的簇大概率是记录者「我」,linked 非空表示已确认身份),candidates(声纹库候选人:person_id 与姓名),sampled(带 speaker 标注的会议片段,paragraph_index 是全文绝对段落下标)。任务:只为 linked 为空或存在矛盾证据的簇推断真实身份;每条推断给出簇号、身份(二选一:candidates 里的 person_id,或候选外的新名字 new_name——new_name 必须有自我介绍级证据)、自报 confidence(high|medium|low)、以及证据列表。证据 type 只能是:self_intro(该簇自我介绍,如「我是张伟」,quote 必须包含所指认的名字)、addressed_reply(称呼应答配对:必须给两条证据——一条在其它簇里称呼该名字,一条是该簇的应答)、third_person_exclusion(该簇以第三人称谈及某人,证明该簇不是那个人)、role_topic(角色/主题弱线索)。所有证据必须逐字存在:paragraph_index 用输入标注的绝对下标,start/end 是该段落的 Unicode scalar(char)半开区间,quote 必须逐字符精确等于该区间,禁止改写、缩略或拼接。输入可能含 calendar 字段(当场会议的日历事件:标题与参会人名单,「(我)」标注记录者本人):参会人是强先验候选,但允许临时加入者/代参会,不是硬约束,不得仅凭名单在无文本证据时强行认人。输出 JSON:{"assignments":[{"cluster":"R2","person_id":"P3","new_name":null,"confidence":"high","evidence":[{"paragraph_index":0,"start":0,"end":4,"quote":"我是张伟","type":"self_intro"}]}]}。person_id 与 new_name 恰好一个非空。没有可靠推断输出 {"assignments":[]}。禁止为已明确关联且无矛盾证据的簇输出条目,禁止仅凭主题相似强行认人。"#;

// ─────────────────────────────────────────────────────────────────────────────
// 三、Agent 执行体(claude / gemini / codex CLI)的提示词 —— 消费方 refine/agent.rs
//
// Agent 路径没有 system/user 之分,整段作为单条 prompt 投递(走 stdin,不进 argv)。
// 各家 CLI 对 MCP 工具的暴露名前缀不同(claude 是 mcp__server__tool,gemini 是裸名),
// 提示词里一律只用裸名,由各家自行映射。
// ─────────────────────────────────────────────────────────────────────────────

/// 「测试运行」连通性探测:不依赖任何笔记,只验证 CLI 能启动并产出。
pub const AGENT_PROBE: &str = r#"只回复两个字:正常。不要任何解释。"#;

/// Aing 指令(精修 + 图谱)。与 [`REFINE_SYSTEM`] 同一套四类修订规则,
/// 但流程改为「读稿 → 修订 → 工具写回」。
pub fn agent_refine(note_id: &str) -> String {
    format!(
        r#"你是会议逐字稿精修与语义图谱助手。任务:完成 voice-notes 笔记 {note_id} 的文本与图谱 Aing。
步骤:
1. 调用 MCP 工具 get_note,参数 {{"note_id":"{note_id}","format":"segments"}},取返回的 paragraphs 数组(段落下标从 0 计;若返回 refined=false 说明还没有精修稿,直接结束并说明)。每个段落带 speaker/name 字段(该段说话人的簇号与人名);精修正文时利用它做人名/称呼错字判断与称呼一致性(如称呼「小王」后由王某的段应答,可确认「王」字写法)。禁止修改说话人归属,禁止据此改写句式或把代词替换成人名,禁止把说话人名当前缀写进正文。
2. 逐段检查,只做四类修订,除此之外禁止任何改动(不改句式和语义,不合并/拆分段落):
a) 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文;b) 实体归一:同一人名/产品名/术语全文统一为最常见写法;c) 删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留「吧」「啊」等语气词;d) 英文与中文之间加空格,产品名保持原大小写。
3. 调用 MCP 工具 apply_refined_texts 一次性写回,参数 {{"note_id":"{note_id}","updates":[{{"index":段落下标,"text":"该段修订后的完整文本"}},...],"model":"你的模型名"}};只提交有改动的段落;若全文确无需要修订,updates 传空数组 []。
4. 文本写回成功后才调用 get_aing_context({{"note_id":"{note_id}"}}重新读取最终 paragraphs、source_seqs、实体/mention、core_predicates、contract_version 与当前 source_hash。不得使用步骤 1 的旧文本做证据。
5. 基于该 context 抽取实体和有原文证据的关系。predicate 只用 core_predicates 或带非空 label 的 custom;evidence 的 paragraph_index 与 Unicode scalar start/end 必须指向最终段落,quote 必须逐字符精确匹配,source_seqs/source_hash 必须照 context 当前值提交。
6. 只调用一次 apply_aing_graph,提交 note_id、entities、relations、contract_version、model。每个 entity 给一个本次载荷内唯一的临时 id,关系 subject/object 引用这些临时 id;服务端会重算全部持久 ID 与 mentions。没有可靠关系时 relations 传空数组 []，仍须提交实体和图谱完成态。
只允许使用 get_note、apply_refined_texts、get_aing_context、apply_aing_graph 四个 MCP 工具;不要读写任何文件,不要执行任何命令。完成后回复一行「完成」即可。"#
    )
}

/// 关系补建(正文不动)。Agent 版的 [`RELATION_ONLY_SYSTEM`]。
pub fn agent_relation(note_id: &str, model: &str) -> String {
    format!(
        r#"你是会议语义关系补建助手。只处理 voice-notes 笔记 {note_id} 的关系，禁止修改正文或使用任何文件/shell 工具。
1. 只调用一次 get_aing_context({{"note_id":"{note_id}"}})，读取最终 paragraphs、source_seqs、entities、core_predicates、contract_version 与 source_hash。
2. entities 必须逐项、原顺序、原 name/kind/aliases 完整照抄 context，不得新增、删除、重排或归一。
3. 仅抽取有逐字证据的 relations；subject/object 引用上述 entities 的临时 id；predicate 只用 core_predicates 或带非空 label 的 custom；evidence 的 paragraph_index 与 Unicode scalar start/end 必须精确指向最终段落，quote 必须逐字符匹配，source_seqs/source_hash 必须照 context 当前值提交。
4. 只调用一次 apply_aing_graph，提交 note_id、原样 entities、relations、context 的 contract_version，以及 model="{model}"。没有可靠关系时 relations 传 []。
全程只允许调用 get_aing_context 与 apply_aing_graph 两个 MCP 工具。完成后回复一行「完成」。"#
    )
}

/// identify 的 Agent 包装:输入全内嵌(Agent 不读库不读盘),输出用哨兵标记包住
/// (允许多行 JSON——serde 不在乎换行,强求单行只会降服从率)。
pub fn agent_identify(ctx_json: &str, start_tag: &str, end_tag: &str) -> String {
    format!(
        "{sys}\n\n输入 JSON 如下:\n{ctx}\n\n输出约定:完成推断后,把结果 JSON 输出一次,并用 {start} 与 {end} 这一对标记包住(标记各占一行,JSON 可以多行);除这一处外不要在任何位置输出这两个标记;不要调用任何工具,不要读写任何文件,不要执行任何命令。",
        sys = IDENTIFY_SYSTEM,
        ctx = ctx_json,
        start = start_tag,
        end = end_tag,
    )
}

/// 起笔记标题(Agent 路径)。无 system 位,约束与正文拼在同一条 prompt 里。
pub fn agent_title(text: &str) -> String {
    format!(
        "只输出一个不超过 12 个字的中文标题,概括下面这场对话的核心主题;不要引号、标点或任何解释。\n\n{text}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// user 侧模板的确切形状。`REFINE_SYSTEM` 里的契约(evidence 回引
    /// `paragraph_index`、`speaker=` 只许用于错字判断)全部依赖这两行的排版,
    /// 改一个冒号或空格就会让模型对不上号,故逐字钉死。
    #[test]
    fn refine_user_message_shape_is_pinned() {
        assert_eq!(
            refine_paragraph_line(7, "张伟", "第八段"),
            "paragraph_index=7 speaker=张伟: 第八段\n"
        );
        let glossary = serde_json::json!({"肯计": "肯定"});
        assert_eq!(
            refine_user(&glossary, "paragraph_index=0 speaker=R2: 正文\n"),
            "术语表(沿用并可扩充):{\"肯计\":\"肯定\"}\n段落:\nparagraph_index=0 speaker=R2: 正文\n"
        );
    }

    /// 护栏:提示词不得在消费方文件里重新内联。
    ///
    /// 集中的价值在于「一处 review 到全部」,而它最常见的失效方式就是有人图省事
    /// 又在 llm.rs / agent.rs 里塞一条新提示词。判据取两条**复发形态**(不求完备):
    /// - HTTP 路径:`"role": "system"` 的 content 直接给字面量而不是 `prompts::`;
    /// - Agent 路径:以「你是…」「你为…」开场的角色设定字面量。
    ///
    /// 惯例同 lib.rs 的 `include_str!("lib.rs")` 源码自审。
    #[test]
    fn prompts_are_not_inlined_in_consumers() {
        let consumers = [
            ("llm.rs", include_str!("llm.rs")),
            ("agent.rs", include_str!("agent.rs")),
            ("identify.rs", include_str!("identify.rs")),
            ("relations.rs", include_str!("relations.rs")),
            ("recluster.rs", include_str!("recluster.rs")),
            ("backfill.rs", include_str!("backfill.rs")),
        ];
        for (name, src) in consumers {
            for (lineno, line) in src.lines().enumerate() {
                let at = format!("{name}:{}", lineno + 1);
                // 只认 json! 宏里真正的消息构造(同一行既给 role 又给 content),
                // 否则会误伤测试里 mock 响应的 `{"message":{"content":..}}`。
                if line.contains(r#""role""#) {
                    if let Some((_, v)) = line.split_once(r#""content":"#) {
                        let v = v.trim();
                        assert!(
                            !v.starts_with('"') && !v.starts_with("r#\""),
                            "{at} 的 role/content 消息直接内联了提示词字面量,请移进 refine/prompts.rs"
                        );
                    }
                }
                assert!(
                    !line.contains("\"你是会议") && !line.contains("\"你为会议"),
                    "{at} 内联了角色设定提示词,请移进 refine/prompts.rs"
                );
            }
        }
    }
}
