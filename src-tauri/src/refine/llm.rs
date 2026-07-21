//! A2 LLM Aing:OpenAI 兼容 chat completions,分块+术语表前传,失败块保原文。

use crate::store::{RefinedParagraph, RelationPredicate};
use serde::Deserialize;
use serde_json::{json, Value};

pub const CHUNK_CHARS: usize = 3000;
pub const REQ_TIMEOUT_S: u64 = 60;
/// 「测试连接」探测的超时:比生产 REQ_TIMEOUT_S 短,测试不该久等。
pub const PROBE_TIMEOUT_S: u64 = 15;

const SYSTEM_PROMPT: &str = "你是会议逐字稿精修助手。对输入的每个段落做四件事,除此之外禁止任何改动:\n1. 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文,禁止改写句式或语义;\n2. 实体归一:同一人名/产品名/术语全文统一为最常见或术语表给定的写法;\n3. 轻度清理口头语:删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留语气词「吧」「啊」等;\n4. 英文与数字排版:英文词组与中文之间加空格,产品名保持原大小写。\n此外,抽取本批出现的关键实体(不改动正文),用修订后的规范名,并抽取有原文证据的语义关系。关系 predicate.type 只能是 participates_in、responsible_for、belongs_to、uses、depends_on、produces、assigned_to、occurs_at,或 custom;custom 必须提供非空 label。每条关系给出 0 到 1 的 confidence,valid_from/valid_to 可为 null。evidence.paragraph_index 必须使用输入中标注的全文绝对段落下标,绝不能改成块内下标;start/end 是该修订后段落的 Unicode scalar(char)半开区间,不是 UTF-8 字节偏移;quote 必须逐字符精确等于该区间。\n输出 JSON:{\"glossary\":{\"错误写法\":\"统一写法\"},\"texts\":[\"段落1修订文\",\"段落2修订文\"],\"entities\":[{\"name\":\"规范名\",\"kind\":\"person|org|project|term|decision|task|place|date\",\"aliases\":[\"别名\"]}],\"relations\":[{\"subject\":\"张三\",\"predicate\":{\"type\":\"responsible_for\",\"label\":null},\"object\":\"灯塔计划\",\"confidence\":0.92,\"valid_from\":null,\"valid_to\":null,\"evidence\":[{\"paragraph_index\":0,\"start\":0,\"end\":8,\"quote\":\"张三负责灯塔计划\"}]}]}。\ntexts 数组长度必须与输入段落数一致,顺序一致。glossary 只收实体类归一项。entities 没有可给空数组,aliases 可省略。relations 必须存在,没有关系时给显式空数组。";

pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

pub enum LlmOutcome {
    Done,
    /// 文本和实体可用，但至少一块缺少/损坏 relations；只降级关系阶段。
    DoneWithRelationErrors,
    Partial(usize),
    Failed,
}

impl LlmOutcome {
    pub(crate) fn relations_complete(&self) -> bool {
        matches!(self, Self::Done)
    }
}

/// 大模型每块吐出的原始实体(未去重、未分配 id)。解析层(refine/mod.rs)再规范化。
#[derive(Debug, Clone, PartialEq)]
pub struct RawEntity {
    pub name: String,
    pub kind: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawRelation {
    pub subject: String,
    pub predicate: RelationPredicate,
    pub object: String,
    pub confidence: f64,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_to: Option<String>,
    pub evidence: Vec<RawEvidence>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawEvidence {
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
}

/// 按累计字符预算切块,返回每块的段落下标。单段超预算独占一块。
pub(crate) fn chunk_indices(ps: &[RefinedParagraph]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut budget = 0usize;
    for (i, p) in ps.iter().enumerate() {
        let n = p.text.chars().count();
        if !cur.is_empty() && budget + n > CHUNK_CHARS {
            out.push(std::mem::take(&mut cur));
            budget = 0;
        }
        cur.push(i);
        budget += n;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// HTTP 状态码 → 归类原因(供「测试连接」按钮显示具体原因)。纯函数,可单测。
pub fn classify_http_status(status: u16) -> &'static str {
    match status {
        401 | 403 => "认证失败(API Key 无效或无权限)",
        404 => "模型不存在或接口地址错误",
        429 => "触发限流",
        s if s >= 500 => "服务端错误",
        _ => "返回异常",
    }
}

/// 火山方舟(Volcengine Ark)的深度思考模型(doubao-seed 等)默认走思维链:单批
/// refine 常需 100s+(思维链先行)而撞破 REQ_TIMEOUT_S 超时。方舟 OpenAI 兼容接口
/// 支持 `thinking:{type:disabled}` 关闭思考。仅对方舟端点注入——其它 OpenAI 兼容
/// provider(OpenAI/DeepSeek/Kimi…)不认此顶层字段,注入可能 400,故按 host 判定。
pub(crate) fn apply_thinking_off(base_url: &str, body: &mut Value) {
    if base_url.to_ascii_lowercase().contains("volces.com") {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("thinking".to_string(), json!({ "type": "disabled" }));
        }
    }
}

/// 「测试连接」:发一条最小 chat/completions,验证端点可达 + 鉴权通过 + 模型可用。
/// 成功返回简短摘要;失败返回归类原因。不落 AI 日志(测试噪音不入库)。
pub fn probe(cfg: &LlmConfig) -> Result<String, String> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut body_json = json!({
        "model": cfg.model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "回复 OK" }],
    });
    apply_thinking_off(&cfg.base_url, &mut body_json);
    let body = body_json.to_string();
    let resp = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_S))
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(&body);
    match resp {
        Ok(r) => {
            let txt = r.into_string().map_err(|e| format!("读取响应失败: {e}"))?;
            let v: Value = serde_json::from_str(&txt)
                .map_err(|_| "返回非 JSON,可能不是 OpenAI 兼容接口".to_string())?;
            if v["choices"][0]["message"]["content"].is_string() {
                Ok(format!("连接正常,模型 {} 可用", cfg.model))
            } else {
                Err("返回内容异常(缺 choices[0].message.content)".to_string())
            }
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(format!("{}(HTTP {code})", classify_http_status(code)))
        }
        Err(ureq::Error::Transport(t)) => {
            let s = t.to_string();
            if s.contains("timed out") || s.contains("timeout") {
                Err("连接超时".to_string())
            } else {
                Err(format!("连不上端点:{s}"))
            }
        }
    }
}

/// 分块调用失败的两种性质:
/// - `Network`:连不上/传输中断(请求根本没有走到应用层),多块场景下若**全部**分块
///   都是这一类,判定整体不可用 → `LlmOutcome::Failed`;
/// - `Content`:HTTP 已应答但内容不可用(非法 JSON、缺字段、长度不符),说明服务本身
///   可达,只是这一块结果不可信,始终计入 `Partial`,即便它是唯一的一块。
enum ChunkErr {
    Network(anyhow::Error),
    Content(anyhow::Error),
}

fn format_chunk_paragraphs(paragraphs: &[(usize, &str)]) -> String {
    paragraphs
        .iter()
        .map(|(absolute_index, text)| format!("paragraph_index={absolute_index}: {text}\n"))
        .collect()
}

impl std::fmt::Display for ChunkErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkErr::Network(e) | ChunkErr::Content(e) => write!(f, "{e}"),
        }
    }
}

fn call_chunk(
    cfg: &LlmConfig,
    glossary: &Value,
    paragraphs: &[(usize, &str)],
    log: Option<&crate::ailog::Ctx>,
) -> Result<(Value, Vec<String>, Vec<RawEntity>, Vec<RawRelation>, bool), ChunkErr> {
    let numbered = format_chunk_paragraphs(paragraphs);
    let user = format!("术语表(沿用并可扩充):{glossary}\n段落:\n{numbered}");
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut body_json = json!({
        "model": cfg.model,
        "temperature": 0.1,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user },
        ],
    });
    apply_thinking_off(&cfg.base_url, &mut body_json);
    let started = std::time::Instant::now();
    let result = do_call_chunk(cfg, &url, &body_json.to_string(), paragraphs.len());
    // AI 日志:请求体全量(key 在请求头,天然不落);响应记服务端原文或错误。
    if let Some(ctx) = log {
        let (response, status, error) = match &result {
            Ok((raw, _, _, _, _, _)) => (Value::String(raw.clone()), "ok", None),
            Err(e) => (Value::Null, "error", Some(e.to_string())),
        };
        crate::ailog::record(
            ctx,
            crate::ailog::Draft {
                kind: "refine_chunk",
                provider: "openai".into(),
                model: Some(cfg.model.clone()),
                endpoint: Some(url.clone()),
                request: body_json,
                response,
                status,
                error,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
    }
    result.map(|(_, glossary, texts, ents, relations, relations_valid)| {
        (glossary, texts, ents, relations, relations_valid)
    })
}

/// 宽松解析实体数组:非数组 → 空;逐项跳过缺 name 的;kind 缺省 "term";aliases 缺省空。
/// 绝不返回错误——实体是增值层,坏数据只当没有,不拖垮 texts。
fn parse_raw_entities(v: &Value) -> Vec<RawEntity> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let name = e["name"].as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let kind = e["kind"].as_str().unwrap_or("term").trim();
            let kind = if kind.is_empty() { "term" } else { kind };
            let aliases = e["aliases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            Some(RawEntity {
                name: name.to_string(),
                kind: kind.to_string(),
                aliases,
            })
        })
        .collect()
}

/// 网络+解析的本体。relations 缺失/损坏时仍返回可信 texts/entities，并用最后一个
/// bool 把失败交给独立关系阶段；显式 [] 则是成功的空关系集合。
fn do_call_chunk(
    cfg: &LlmConfig,
    url: &str,
    body: &str,
    expect_len: usize,
) -> Result<
    (
        String,
        Value,
        Vec<String>,
        Vec<RawEntity>,
        Vec<RawRelation>,
        bool,
    ),
    ChunkErr,
> {
    let resp_text = ureq::post(url)
        .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(body)
        .map_err(|e| ChunkErr::Network(e.into()))?
        .into_string()
        .map_err(|e| ChunkErr::Network(e.into()))?;
    let resp: Value = serde_json::from_str(&resp_text).map_err(|e| ChunkErr::Content(e.into()))?;
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ChunkErr::Content(anyhow::anyhow!("响应缺 choices[0].message.content")))?;
    let parsed: Value = serde_json::from_str(content).map_err(|e| ChunkErr::Content(e.into()))?;
    let texts_out: Vec<String> = parsed["texts"]
        .as_array()
        .ok_or_else(|| ChunkErr::Content(anyhow::anyhow!("响应缺 texts 数组")))?
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    if texts_out.len() != expect_len {
        return Err(ChunkErr::Content(anyhow::anyhow!(
            "texts 长度不符: 期望 {} 实得 {}",
            expect_len,
            texts_out.len()
        )));
    }
    let entities = parse_raw_entities(&parsed["entities"]);
    let (relations, relations_valid) = match parsed.get("relations") {
        Some(value) => match serde_json::from_value::<Vec<RawRelation>>(value.clone()) {
            Ok(relations) => (relations, true),
            Err(error) => {
                eprintln!("refine llm: relations 解析失败: {error}");
                (Vec::new(), false)
            }
        },
        None => {
            eprintln!("refine llm: 响应缺 relations 数组");
            (Vec::new(), false)
        }
    };
    Ok((
        resp_text,
        parsed["glossary"].clone(),
        texts_out,
        entities,
        relations,
        relations_valid,
    ))
}

/// 为整场笔记生成主题标题(Aing 完成后调用,替换未被用户改过的默认标题)。
/// 单次请求、失败即放弃:标题是锦上添花,不进 stages、不重试、不影响 Aing 结果。
pub fn gen_title(
    cfg: &LlmConfig,
    paragraphs: &[RefinedParagraph],
    log: Option<&crate::ailog::Ctx>,
) -> anyhow::Result<String> {
    let mut text = String::new();
    for p in paragraphs {
        if text.chars().count() > 1500 {
            break;
        }
        text.push_str(&p.text);
        text.push('\n');
    }
    if text.trim().is_empty() {
        anyhow::bail!("修订稿无内容,不生成标题");
    }
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut body_json = json!({
        "model": cfg.model,
        "temperature": 0.3,
        "messages": [
            { "role": "system", "content": "你为会议转写起标题。只输出一个不超过 12 个字的中文标题,概括这场对话的核心主题;不要引号、标点或任何解释。" },
            { "role": "user", "content": text },
        ],
    });
    apply_thinking_off(&cfg.base_url, &mut body_json);
    let started = std::time::Instant::now();
    let resp_result: anyhow::Result<String> = (|| {
        Ok(ureq::post(&url)
            .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
            .set("authorization", &format!("Bearer {}", cfg.api_key))
            .set("content-type", "application/json")
            .send_string(&body_json.to_string())?
            .into_string()?)
    })();
    if let Some(ctx) = log {
        let (response, status, error) = match &resp_result {
            Ok(raw) => (Value::String(raw.clone()), "ok", None),
            Err(e) => (Value::Null, "error", Some(e.to_string())),
        };
        crate::ailog::record(
            ctx,
            crate::ailog::Draft {
                kind: "title",
                provider: "openai".into(),
                model: Some(cfg.model.clone()),
                endpoint: Some(url.clone()),
                request: body_json,
                response,
                status,
                error,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
    }
    let resp_text = resp_result?;
    let resp: Value = serde_json::from_str(&resp_text)?;
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应缺 choices[0].message.content"))?;
    let title = content
        .trim()
        .trim_matches(['"', '“', '”', '「', '」', '《', '》', '。'])
        .trim()
        .to_string();
    // 长度守卫:空或离谱地长(不服从指令,可能把解释吐出来了)都放弃。
    if title.is_empty() || title.chars().count() > 20 || title.contains('\n') {
        anyhow::bail!("标题不合规,放弃: {title:?}");
    }
    Ok(title)
}

/// 逐块 Aing,glossary 串行前传。全部成功 Done;有内容级失败(或网络与内容混合失败)
/// 计入 Partial;全部分块都是网络级失败(服务完全不可达)判 Failed。
/// log=Some 时每个分块的请求/响应记入 AI 日志(旁路,失败不影响 Aing)。
pub fn polish(
    cfg: &LlmConfig,
    paragraphs: &mut [RefinedParagraph],
    log: Option<&crate::ailog::Ctx>,
) -> (LlmOutcome, Vec<RawEntity>, Vec<RawRelation>) {
    let chunks = chunk_indices(paragraphs);
    if chunks.is_empty() {
        return (LlmOutcome::Done, Vec::new(), Vec::new());
    }
    let mut glossary = json!({});
    let mut failed = 0usize;
    let mut network_failed = 0usize;
    let mut all_entities: Vec<RawEntity> = Vec::new();
    let mut all_relations: Vec<RawRelation> = Vec::new();
    let mut relation_failed = false;
    for chunk in &chunks {
        let inputs: Vec<(usize, &str)> = chunk
            .iter()
            .map(|&i| (i, paragraphs[i].text.as_str()))
            .collect();
        match call_chunk(cfg, &glossary, &inputs, log) {
            Ok((g, outs, ents, relations, relations_valid)) => {
                if let Value::Object(map) = g {
                    if let Value::Object(acc) = &mut glossary {
                        acc.extend(map);
                    }
                }
                for (&i, t) in chunk.iter().zip(outs) {
                    if !t.trim().is_empty() {
                        paragraphs[i].text = t;
                    }
                }
                all_entities.extend(ents);
                if relations_valid {
                    all_relations.extend(relations);
                } else {
                    relation_failed = true;
                }
            }
            Err(e) => {
                if matches!(e, ChunkErr::Network(_)) {
                    network_failed += 1;
                }
                eprintln!("refine llm: 块失败保留原文: {e}");
                failed += 1;
                relation_failed = true;
            }
        }
    }
    let outcome = if failed == 0 && relation_failed {
        LlmOutcome::DoneWithRelationErrors
    } else if failed == 0 {
        LlmOutcome::Done
    } else if network_failed == chunks.len() {
        LlmOutcome::Failed
    } else {
        LlmOutcome::Partial(failed)
    };
    (outcome, all_entities, all_relations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// 起一个只响应一次的本地 mock,返回给定 body 的 200 JSON。
    /// 循环读取直到看到 \r\n\r\n 头结束(ureq 可能分多次写 header/body),
    /// 请求体本身不解析(mock 不依赖具体请求内容),只需保证握手完整后再回包。
    fn mock_server(responses: Vec<String>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in responses {
                let (mut s, _) = listener.accept().unwrap();
                read_request(&mut s);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// 读到请求头结束(\r\n\r\n)且(若有 Content-Length)body 也读完为止;丢弃内容。
    fn read_request(s: &mut std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(header_end) = find_header_end(&buf) {
                        let header_str = String::from_utf8_lossy(&buf[..header_end]);
                        let content_length = header_str
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.trim().eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        let body_have = buf.len() - (header_end + 4);
                        if body_have >= content_length {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn chat_body(texts: &[&str], glossary: &str) -> String {
        let content = serde_json::json!({
            "glossary": serde_json::from_str::<serde_json::Value>(glossary).unwrap(),
            "texts": texts,
            "relations": [],
        })
        .to_string();
        serde_json::json!({ "choices": [{ "message": { "content": content } }] }).to_string()
    }

    fn para(text: &str) -> crate::store::RefinedParagraph {
        crate::store::RefinedParagraph {
            speaker: "R1".into(),
            name: None,
            person_id: None,
            start_ms: 0,
            end_ms: 1000,
            text: text.into(),
            source_seqs: vec![0],
            mentions: vec![],
        }
    }

    #[test]
    fn thinking_off_only_for_volces_ark() {
        // 方舟端点:注入 thinking:disabled
        let mut b = json!({ "model": "m" });
        apply_thinking_off("https://ark.cn-beijing.volces.com/api/v3", &mut b);
        assert_eq!(b["thinking"]["type"], "disabled");
        // 大小写不敏感
        let mut b2 = json!({ "model": "m" });
        apply_thinking_off("HTTPS://ARK.CN-BEIJING.VOLCES.COM/api/v3", &mut b2);
        assert_eq!(b2["thinking"]["type"], "disabled");
        // 其它 OpenAI 兼容 provider:绝不注入(否则可能 400)
        for u in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com",
            "https://api.moonshot.cn/v1",
        ] {
            let mut o = json!({ "model": "m" });
            apply_thinking_off(u, &mut o);
            assert!(
                o.get("thinking").is_none(),
                "非方舟端点不应注入 thinking: {u}"
            );
        }
    }

    #[test]
    fn polish_rewrites_texts_on_success() {
        let base = mock_server(vec![chat_body(&["我们肯定要做。"], r#"{"肯计":"肯定"}"#)]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("我们肯计要做。")];
        let (outcome, _ents, _relations) = polish(&cfg, &mut ps, None);
        assert!(matches!(outcome, LlmOutcome::Done));
        assert_eq!(ps[0].text, "我们肯定要做。");
    }

    #[test]
    fn length_mismatch_keeps_originals_as_partial() {
        let base = mock_server(vec![chat_body(
            &["只有一段", "但输入两段之外多了一段"],
            "{}",
        )]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("原文一")];
        let (outcome, _ents, _relations) = polish(&cfg, &mut ps, None);
        assert!(matches!(outcome, LlmOutcome::Partial(1)));
        assert_eq!(ps[0].text, "原文一", "长度不符必须保留原文");
    }

    #[test]
    fn connection_refused_is_failed_and_keeps_originals() {
        let cfg = LlmConfig {
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("原文")];
        let (outcome, _ents, _relations) = polish(&cfg, &mut ps, None);
        assert!(matches!(outcome, LlmOutcome::Failed));
        assert_eq!(ps[0].text, "原文");
    }

    /// AI 日志:成功块记完整请求体+响应原文,失败块记 error;key 绝不落盘。
    #[test]
    fn polish_logs_request_and_response_per_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ailog::Ctx {
            data_root: tmp.path().to_path_buf(),
            note_id: "n1".into(),
        };
        let base = mock_server(vec![chat_body(&["修订。"], "{}")]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "SECRET-KEY".into(),
        };
        let mut ps = vec![para("原文。")];
        let (outcome, _ents, _relations) = polish(&cfg, &mut ps, Some(&ctx));
        assert!(matches!(outcome, LlmOutcome::Done));
        // 连不上的一轮:同样要留痕
        let cfg_bad = LlmConfig {
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps2 = vec![para("原文。")];
        let (outcome2, _ents2, _relations2) = polish(&cfg_bad, &mut ps2, Some(&ctx));
        assert!(matches!(outcome2, LlmOutcome::Failed));
        let v = crate::ailog::query(tmp.path(), &crate::ailog::Filter::default());
        assert_eq!(v["total"], 2);
        let all = serde_json::to_string(&v);
        assert!(!all.unwrap().contains("SECRET-KEY"), "api key 绝不落日志");
        let entries = v["entries"].as_array().unwrap();
        let ok = entries.iter().find(|e| e["status"] == "ok").unwrap();
        assert_eq!(ok["kind"], "refine_chunk");
        assert_eq!(ok["note_id"], "n1");
        assert!(
            ok["request"]["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("原文。"),
            "请求体全量"
        );
        assert!(
            ok["response"].as_str().unwrap().contains("choices"),
            "响应原文全量"
        );
        let err = entries.iter().find(|e| e["status"] == "error").unwrap();
        assert!(err["error"].as_str().unwrap().len() > 0);
    }

    #[test]
    fn parses_entities_from_response() {
        let body = r#"{"choices":[{"message":{"content":"{\"glossary\":{},\"texts\":[\"灯塔计划下周启动\"],\"entities\":[{\"name\":\"灯塔计划\",\"kind\":\"project\",\"aliases\":[\"Lighthouse\"]}],\"relations\":[]}"}}]}"#;
        let base = mock_server(vec![body.to_string()]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("灯塔计划下周启动")];
        let (outcome, ents, _relations) = polish(&cfg, &mut ps, None);
        assert!(matches!(outcome, LlmOutcome::Done));
        assert_eq!(ps[0].text, "灯塔计划下周启动");
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].name, "灯塔计划");
        assert_eq!(ents[0].kind, "project");
        assert_eq!(ents[0].aliases, vec!["Lighthouse".to_string()]);
    }

    #[test]
    fn missing_entities_key_degrades_to_empty_without_failing_texts() {
        let body = r#"{"choices":[{"message":{"content":"{\"glossary\":{},\"texts\":[\"你好\"],\"relations\":[]}"}}]}"#;
        let base = mock_server(vec![body.to_string()]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("你好")];
        let (outcome, ents, _relations) = polish(&cfg, &mut ps, None);
        assert!(
            matches!(outcome, LlmOutcome::Done),
            "缺 entities 不影响 texts 成败"
        );
        assert!(ents.is_empty());
    }

    #[test]
    fn parses_relations_with_absolute_unicode_scalar_offsets() {
        let content = serde_json::json!({
            "glossary": {},
            "texts": ["🙂张三负责灯塔计划"],
            "entities": [
                {"name": "张三", "kind": "person", "aliases": []},
                {"name": "灯塔计划", "kind": "project", "aliases": []}
            ],
            "relations": [{
                "subject": "张三",
                "predicate": {"type": "responsible_for", "label": null},
                "object": "灯塔计划",
                "confidence": 0.92,
                "valid_from": null,
                "valid_to": null,
                "evidence": [{
                    "paragraph_index": 0,
                    "start": 1,
                    "end": 9,
                    "quote": "张三负责灯塔计划"
                }]
            }]
        })
        .to_string();
        let body = serde_json::json!({
            "choices": [{"message": {"content": content}}]
        })
        .to_string();
        let base = mock_server(vec![body]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("🙂张三负则灯塔计划")];

        let (outcome, ents, relations) = polish(&cfg, &mut ps, None);

        assert!(matches!(outcome, LlmOutcome::Done));
        assert_eq!(ps[0].text, "🙂张三负责灯塔计划");
        assert_eq!(ents.len(), 2);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].predicate.kind, "responsible_for");
        assert_eq!(relations[0].evidence[0].paragraph_index, 0);
        assert_eq!(
            (relations[0].evidence[0].start, relations[0].evidence[0].end),
            (1, 9)
        );
        assert_eq!(relations[0].evidence[0].quote, "张三负责灯塔计划");
    }

    #[test]
    fn missing_relations_is_graph_only_failure() {
        let content = serde_json::json!({
            "glossary": {},
            "texts": ["修订文本"],
            "entities": [{"name": "修订", "kind": "term", "aliases": []}]
        })
        .to_string();
        let body = serde_json::json!({
            "choices": [{"message": {"content": content}}]
        })
        .to_string();
        let base = mock_server(vec![body]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut ps = vec![para("原始文本")];

        let (outcome, ents, relations) = polish(&cfg, &mut ps, None);

        assert!(matches!(outcome, LlmOutcome::DoneWithRelationErrors));
        assert_eq!(ps[0].text, "修订文本", "关系字段缺失不能回滚文本");
        assert_eq!(ents.len(), 1, "关系字段缺失不能回滚实体解析");
        assert!(relations.is_empty());
    }

    #[test]
    fn malformed_relations_is_graph_only_failure_but_explicit_empty_is_success() {
        let malformed = serde_json::json!({
            "choices": [{"message": {"content": serde_json::json!({
                "glossary": {}, "texts": ["修订一"], "entities": [], "relations": {}
            }).to_string()}}]
        })
        .to_string();
        let empty = chat_body(&["修订二"], "{}");
        let base = mock_server(vec![malformed, empty]);
        let cfg = LlmConfig {
            base_url: base,
            model: "m".into(),
            api_key: "k".into(),
        };
        let mut first = vec![para("原文一")];
        let mut second = vec![para("原文二")];

        let (bad, _, bad_relations) = polish(&cfg, &mut first, None);
        let (good, _, empty_relations) = polish(&cfg, &mut second, None);

        assert!(matches!(bad, LlmOutcome::DoneWithRelationErrors));
        assert_eq!(first[0].text, "修订一");
        assert!(bad_relations.is_empty());
        assert!(matches!(good, LlmOutcome::Done));
        assert_eq!(second[0].text, "修订二");
        assert!(empty_relations.is_empty());
    }

    #[test]
    fn chunking_respects_char_budget() {
        let texts: Vec<String> = (0..4).map(|i| "字".repeat(1600 + i)).collect();
        let ps: Vec<_> = texts.iter().map(|t| para(t)).collect();
        let chunks = chunk_indices(&ps);
        assert!(chunks.len() >= 2, "4×1600 字必须切多块");
        for c in &chunks {
            let total: usize = c.iter().map(|&i| ps[i].text.chars().count()).sum();
            assert!(total <= CHUNK_CHARS || c.len() == 1);
        }
    }

    #[test]
    fn chunk_prompt_keeps_absolute_paragraph_indexes() {
        assert_eq!(
            format_chunk_paragraphs(&[(7, "第八段"), (12, "第十三段")]),
            "paragraph_index=7: 第八段\nparagraph_index=12: 第十三段\n"
        );
    }
}

#[cfg(test)]
mod probe_tests {
    use super::classify_http_status;

    #[test]
    fn classify_maps_status_to_reason() {
        assert!(classify_http_status(401).contains("认证"));
        assert!(classify_http_status(403).contains("认证"));
        assert!(classify_http_status(404).contains("模型不存在"));
        assert!(classify_http_status(429).contains("限流"));
        assert!(classify_http_status(500).contains("服务端"));
        assert_eq!(classify_http_status(418), "返回异常");
    }
}
