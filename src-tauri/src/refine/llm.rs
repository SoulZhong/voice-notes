//! A2 LLM 精修:OpenAI 兼容 chat completions,分块+术语表前传,失败块保原文。

use crate::store::RefinedParagraph;
use serde_json::{json, Value};

pub const CHUNK_CHARS: usize = 3000;
pub const REQ_TIMEOUT_S: u64 = 60;

const SYSTEM_PROMPT: &str = "你是会议逐字稿精修助手。对输入的每个段落做四件事,除此之外禁止任何改动:\n1. 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文,禁止改写句式或语义;\n2. 实体归一:同一人名/产品名/术语全文统一为最常见或术语表给定的写法;\n3. 轻度清理口头语:删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留语气词「吧」「啊」等;\n4. 英文与数字排版:英文词组与中文之间加空格,产品名保持原大小写。\n输出 JSON:{\"glossary\":{\"错误写法\":\"统一写法\"},\"texts\":[\"段落1修订文\",\"段落2修订文\"]}。\ntexts 数组长度必须与输入段落数一致,顺序一致。glossary 只收实体类归一项。";

pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

pub enum LlmOutcome {
    Done,
    Partial(usize),
    Failed,
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

/// 分块调用失败的两种性质:
/// - `Network`:连不上/传输中断(请求根本没有走到应用层),多块场景下若**全部**分块
///   都是这一类,判定整体不可用 → `LlmOutcome::Failed`;
/// - `Content`:HTTP 已应答但内容不可用(非法 JSON、缺字段、长度不符),说明服务本身
///   可达,只是这一块结果不可信,始终计入 `Partial`,即便它是唯一的一块。
enum ChunkErr {
    Network(anyhow::Error),
    Content(anyhow::Error),
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
    texts: &[&str],
    log: Option<&crate::ailog::Ctx>,
) -> Result<(Value, Vec<String>), ChunkErr> {
    let numbered: String = texts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {}\n", i + 1, t))
        .collect();
    let user = format!("术语表(沿用并可扩充):{glossary}\n段落:\n{numbered}");
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body_json = json!({
        "model": cfg.model,
        "temperature": 0.1,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user },
        ],
    });
    let started = std::time::Instant::now();
    let result = do_call_chunk(cfg, &url, &body_json.to_string(), texts.len());
    // AI 日志:请求体全量(key 在请求头,天然不落);响应记服务端原文或错误。
    if let Some(ctx) = log {
        let (response, status, error) = match &result {
            Ok((raw, _, _)) => (Value::String(raw.clone()), "ok", None),
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
    result.map(|(_, glossary, texts)| (glossary, texts))
}

/// 网络+解析的本体,返回 (响应原文, glossary, texts) 供 call_chunk 记日志后拆用。
fn do_call_chunk(
    cfg: &LlmConfig,
    url: &str,
    body: &str,
    expect_len: usize,
) -> Result<(String, Value, Vec<String>), ChunkErr> {
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
    Ok((resp_text, parsed["glossary"].clone(), texts_out))
}

/// 为整场笔记生成主题标题(精修完成后调用,替换未被用户改过的默认标题)。
/// 单次请求、失败即放弃:标题是锦上添花,不进 stages、不重试、不影响精修结果。
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
        anyhow::bail!("精修稿无内容,不生成标题");
    }
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body_json = json!({
        "model": cfg.model,
        "temperature": 0.3,
        "messages": [
            { "role": "system", "content": "你为会议转写起标题。只输出一个不超过 12 个字的中文标题,概括这场对话的核心主题;不要引号、标点或任何解释。" },
            { "role": "user", "content": text },
        ],
    });
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

/// 逐块精修,glossary 串行前传。全部成功 Done;有内容级失败(或网络与内容混合失败)
/// 计入 Partial;全部分块都是网络级失败(服务完全不可达)判 Failed。
/// log=Some 时每个分块的请求/响应记入 AI 日志(旁路,失败不影响精修)。
pub fn polish(cfg: &LlmConfig, paragraphs: &mut [RefinedParagraph], log: Option<&crate::ailog::Ctx>) -> LlmOutcome {
    let chunks = chunk_indices(paragraphs);
    if chunks.is_empty() {
        return LlmOutcome::Done;
    }
    let mut glossary = json!({});
    let mut failed = 0usize;
    let mut network_failed = 0usize;
    for chunk in &chunks {
        let texts: Vec<&str> = chunk.iter().map(|&i| paragraphs[i].text.as_str()).collect();
        match call_chunk(cfg, &glossary, &texts, log) {
            Ok((g, outs)) => {
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
            }
            Err(e) => {
                if matches!(e, ChunkErr::Network(_)) {
                    network_failed += 1;
                }
                eprintln!("refine llm: 块失败保留原文: {e}");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        LlmOutcome::Done
    } else if network_failed == chunks.len() {
        LlmOutcome::Failed
    } else {
        LlmOutcome::Partial(failed)
    }
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
        let content = serde_json::json!({ "glossary": serde_json::from_str::<serde_json::Value>(glossary).unwrap(), "texts": texts })
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
        }
    }

    #[test]
    fn polish_rewrites_texts_on_success() {
        let base = mock_server(vec![chat_body(&["我们肯定要做。"], r#"{"肯计":"肯定"}"#)]);
        let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("我们肯计要做。")];
        assert!(matches!(polish(&cfg, &mut ps, None), LlmOutcome::Done));
        assert_eq!(ps[0].text, "我们肯定要做。");
    }

    #[test]
    fn length_mismatch_keeps_originals_as_partial() {
        let base = mock_server(vec![chat_body(&["只有一段", "但输入两段之外多了一段"], "{}")]);
        let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("原文一")];
        assert!(matches!(polish(&cfg, &mut ps, None), LlmOutcome::Partial(1)));
        assert_eq!(ps[0].text, "原文一", "长度不符必须保留原文");
    }

    #[test]
    fn connection_refused_is_failed_and_keeps_originals() {
        let cfg = LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("原文")];
        assert!(matches!(polish(&cfg, &mut ps, None), LlmOutcome::Failed));
        assert_eq!(ps[0].text, "原文");
    }

    /// AI 日志:成功块记完整请求体+响应原文,失败块记 error;key 绝不落盘。
    #[test]
    fn polish_logs_request_and_response_per_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ailog::Ctx { data_root: tmp.path().to_path_buf(), note_id: "n1".into() };
        let base = mock_server(vec![chat_body(&["修订。"], "{}")]);
        let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "SECRET-KEY".into() };
        let mut ps = vec![para("原文。")];
        assert!(matches!(polish(&cfg, &mut ps, Some(&ctx)), LlmOutcome::Done));
        // 连不上的一轮:同样要留痕
        let cfg_bad = LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        let mut ps2 = vec![para("原文。")];
        assert!(matches!(polish(&cfg_bad, &mut ps2, Some(&ctx)), LlmOutcome::Failed));
        let v = crate::ailog::query(tmp.path(), &crate::ailog::Filter::default());
        assert_eq!(v["total"], 2);
        let all = serde_json::to_string(&v);
        assert!(!all.unwrap().contains("SECRET-KEY"), "api key 绝不落日志");
        let entries = v["entries"].as_array().unwrap();
        let ok = entries.iter().find(|e| e["status"] == "ok").unwrap();
        assert_eq!(ok["kind"], "refine_chunk");
        assert_eq!(ok["note_id"], "n1");
        assert!(ok["request"]["messages"][1]["content"].as_str().unwrap().contains("原文。"), "请求体全量");
        assert!(ok["response"].as_str().unwrap().contains("choices"), "响应原文全量");
        let err = entries.iter().find(|e| e["status"] == "error").unwrap();
        assert!(err["error"].as_str().unwrap().len() > 0);
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
}
