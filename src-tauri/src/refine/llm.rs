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

fn call_chunk(cfg: &LlmConfig, glossary: &Value, texts: &[&str]) -> Result<(Value, Vec<String>), ChunkErr> {
    let numbered: String = texts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {}\n", i + 1, t))
        .collect();
    let user = format!("术语表(沿用并可扩充):{glossary}\n段落:\n{numbered}");
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "temperature": 0.1,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user },
        ],
    })
    .to_string();
    let resp_text = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(&body)
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
    if texts_out.len() != texts.len() {
        return Err(ChunkErr::Content(anyhow::anyhow!(
            "texts 长度不符: 期望 {} 实得 {}",
            texts.len(),
            texts_out.len()
        )));
    }
    Ok((parsed["glossary"].clone(), texts_out))
}

/// 为整场笔记生成主题标题(精修完成后调用,替换未被用户改过的默认标题)。
/// 单次请求、失败即放弃:标题是锦上添花,不进 stages、不重试、不影响精修结果。
pub fn gen_title(cfg: &LlmConfig, paragraphs: &[RefinedParagraph]) -> anyhow::Result<String> {
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
    let body = json!({
        "model": cfg.model,
        "temperature": 0.3,
        "messages": [
            { "role": "system", "content": "你为会议转写起标题。只输出一个不超过 12 个字的中文标题,概括这场对话的核心主题;不要引号、标点或任何解释。" },
            { "role": "user", "content": text },
        ],
    })
    .to_string();
    let resp_text = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(&body)?
        .into_string()?;
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
pub fn polish(cfg: &LlmConfig, paragraphs: &mut [RefinedParagraph]) -> LlmOutcome {
    let chunks = chunk_indices(paragraphs);
    if chunks.is_empty() {
        return LlmOutcome::Done;
    }
    let mut glossary = json!({});
    let mut failed = 0usize;
    let mut network_failed = 0usize;
    for chunk in &chunks {
        let texts: Vec<&str> = chunk.iter().map(|&i| paragraphs[i].text.as_str()).collect();
        match call_chunk(cfg, &glossary, &texts) {
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
        assert!(matches!(polish(&cfg, &mut ps), LlmOutcome::Done));
        assert_eq!(ps[0].text, "我们肯定要做。");
    }

    #[test]
    fn length_mismatch_keeps_originals_as_partial() {
        let base = mock_server(vec![chat_body(&["只有一段", "但输入两段之外多了一段"], "{}")]);
        let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("原文一")];
        assert!(matches!(polish(&cfg, &mut ps), LlmOutcome::Partial(1)));
        assert_eq!(ps[0].text, "原文一", "长度不符必须保留原文");
    }

    #[test]
    fn connection_refused_is_failed_and_keeps_originals() {
        let cfg = LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("原文")];
        assert!(matches!(polish(&cfg, &mut ps), LlmOutcome::Failed));
        assert_eq!(ps[0].text, "原文");
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
