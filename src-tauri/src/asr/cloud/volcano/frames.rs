//! 火山引擎 sauc v3 二进制帧编解码(纯函数,不碰网络)。
//!
//! 协议要点(spec §2.1 火山节):
//! - 4 字节帧头:byte0 = 协议版本(高4位)<<4 | 头部大小(低4位),恒为 `0x11`
//!   (版本1、头部 1×4 字节,本实现不用扩展头);byte1 = 消息类型(高4位)<<4 |
//!   标志位(低4位);byte2 = 序列化方式(高4位)<<4 | 压缩方式(低4位);byte3 保留恒 `0x00`。
//! - 客户端帧 = 帧头 + 4字节大端 payload 长度 + payload。
//! - 服务端"完整响应"帧 = 帧头 + 4字节大端 seq(i32) + 4字节大端 payload 长度 + payload。
//! - 服务端"错误"帧 = 帧头(byte1 高4位=0b1111) + 4字节大端错误码(u32) + 4字节大端
//!   消息长度 + UTF-8 消息体(无 gzip,直接文本)。
//! - 大端(big-endian)是协议强制字节序,不是任意选择——与文档给定的 golden 字节
//!   逐字节对齐是唯一判据,不能想当然用本机字节序。

use crate::asr::cloud::{CloudWord, DefiniteUtterance};
use anyhow::{anyhow, bail, Context};
use std::io::Read;

/// 注意 `bigmodel_async` 不是笔误:它是官方推荐的流式优化变体(下行更早吐定稿,
/// 2026-07 文档核实),别"顺手"改回 `bigmodel`。
pub const WS_URL: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
pub const RESOURCE_ID_STREAM: &str = "volc.bigasr.sauc.duration";
pub const FLASH_URL: &str = "https://openspeech.bytedance.com/api/v3/auc/bigmodel/recognize/flash";
pub const RESOURCE_ID_FLASH: &str = "volc.bigasr.auc_turbo";

// 消息类型(byte1 高4位)。
const MSG_TYPE_FULL_REQUEST: u8 = 0b0001;
const MSG_TYPE_AUDIO_ONLY: u8 = 0b0010;
const MSG_TYPE_FULL_RESPONSE: u8 = 0b1001;
const MSG_TYPE_ERROR: u8 = 0b1111;

// 标志位(byte1 低4位)。
const FLAG_NONE: u8 = 0b0000;
const FLAG_LAST_AUDIO: u8 = 0b0010; // 末包:告知服务端音频流结束,触发收尾识别

// 序列化方式(byte2 高4位)。
const SERIAL_NONE: u8 = 0b0000;
const SERIAL_JSON: u8 = 0b0001;

// 压缩方式(byte2 低4位)。
const COMPRESS_NONE: u8 = 0b0000;
const COMPRESS_GZIP: u8 = 0b0001;

/// 版本1 + 头部大小1×4字节,固定值,协议无扩展头场景。
const HEADER_BYTE0: u8 = 0x11;

/// 构造"完整客户端请求"帧:JSON 载荷已在外部 gzip 压缩(体积/带宽考量,
/// 协议按 byte2 低4位标注压缩方式,解码端据此解压)。
pub fn full_request_frame(gzip_json: &[u8]) -> Vec<u8> {
    let byte1 = (MSG_TYPE_FULL_REQUEST << 4) | FLAG_NONE;
    let byte2 = (SERIAL_JSON << 4) | COMPRESS_GZIP;
    let mut out = vec![HEADER_BYTE0, byte1, byte2, 0x00];
    out.extend_from_slice(&(gzip_json.len() as u32).to_be_bytes());
    out.extend_from_slice(gzip_json);
    out
}

/// 构造"纯音频"帧。`last=true` 标记本包为末包(flags=0b0010),服务端收到后
/// 结束当前会话的流式识别、返回收尾的完整结果。音频本身不序列化/不压缩
/// (原始 PCM 直传,序列化=none、压缩=none)。
pub fn audio_frame(pcm: &[u8], last: bool) -> Vec<u8> {
    let flags = if last { FLAG_LAST_AUDIO } else { FLAG_NONE };
    let byte1 = (MSG_TYPE_AUDIO_ONLY << 4) | flags;
    let byte2 = (SERIAL_NONE << 4) | COMPRESS_NONE;
    let mut out = vec![HEADER_BYTE0, byte1, byte2, 0x00];
    out.extend_from_slice(&(pcm.len() as u32).to_be_bytes());
    out.extend_from_slice(pcm);
    out
}

/// 构造"完整客户端请求"的 JSON 载荷(未压缩,调用方负责 gzip 后传给
/// `full_request_frame`)。`app_hotwords` 为逗号分隔的热词字符串,非空时
/// 编码进 `request.corpus.context`(协议要求该字段本身是一段 JSON 字符串,
/// 不是内嵌对象,因此这里手动 `to_string()` 二次序列化)。
pub fn full_request_json(app_hotwords: Option<&str>) -> serde_json::Value {
    let mut request = serde_json::json!({
        "model_name": "bigmodel",
        "enable_itn": true,
        "enable_punc": true,
        "show_utterances": true,
        "result_type": "full",
    });

    if let Some(words) = app_hotwords {
        let hotwords: Vec<serde_json::Value> = words
            .split(',')
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .map(|w| serde_json::json!({"word": w}))
            .collect();
        if !hotwords.is_empty() {
            let context = serde_json::json!({"hotwords": hotwords}).to_string();
            request["corpus"] = serde_json::json!({"context": context});
        }
    }

    serde_json::json!({
        "user": {"uid": "voice-notes"},
        "audio": {
            "format": "pcm",
            "codec": "raw",
            "rate": 16000,
            "bits": 16,
            "channel": 1,
        },
        "request": request,
    })
}

/// 服务端下行帧,解析后二选一。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    Response { seq: i32, json: serde_json::Value },
    Error { code: u32, msg: String },
}

/// 解析服务端二进制帧。分支依据 byte1 高4位(消息类型):
/// - 完整响应:紧跟大端 i32 seq、大端 u32 payload 长度、payload(按 byte2 低4位
///   决定是否需要 gzip 解压后再当 JSON 解析)。
/// - 错误:紧跟大端 u32 错误码、大端 u32 消息长度、UTF-8 消息体(无压缩,协议
///   为了故障场景下即便客户端 gzip 有问题也能读出错误文本)。
pub fn parse_server_frame(buf: &[u8]) -> anyhow::Result<ServerFrame> {
    if buf.len() < 4 {
        bail!("帧过短,不足4字节头部: {} bytes", buf.len());
    }
    let byte1 = buf[1];
    let msg_type = byte1 >> 4;
    let byte2 = buf[2];
    let compression = byte2 & 0x0F;

    match msg_type {
        MSG_TYPE_FULL_RESPONSE => {
            if buf.len() < 12 {
                bail!("完整响应帧过短: {} bytes", buf.len());
            }
            let seq = i32::from_be_bytes(buf[4..8].try_into().unwrap());
            let payload_len = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
            let payload = buf
                .get(12..12 + payload_len)
                .ok_or_else(|| anyhow!("payload 长度声明({payload_len})超出实际帧体"))?;
            let raw = if compression == COMPRESS_GZIP {
                gunzip(payload)?
            } else {
                payload.to_vec()
            };
            let json: serde_json::Value =
                serde_json::from_slice(&raw).context("解析服务端响应 JSON 失败")?;
            Ok(ServerFrame::Response { seq, json })
        }
        MSG_TYPE_ERROR => {
            if buf.len() < 12 {
                bail!("错误帧过短: {} bytes", buf.len());
            }
            let code = u32::from_be_bytes(buf[4..8].try_into().unwrap());
            let msg_len = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
            let msg_bytes = buf
                .get(12..12 + msg_len)
                .ok_or_else(|| anyhow!("错误消息长度声明({msg_len})超出实际帧体"))?;
            let msg = String::from_utf8(msg_bytes.to_vec()).context("错误消息非合法 UTF-8")?;
            Ok(ServerFrame::Error { code, msg })
        }
        other => bail!("未知服务端消息类型: {other:#06b}"),
    }
}

fn gunzip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .context("gzip 解压服务端 payload 失败")?;
    Ok(out)
}

/// 从服务端响应 JSON 中提取:
/// - 整体 interim 文本(`result.text`,流式过程中持续覆盖,不代表最终定稿);
/// - `definite==true` 的分句列表(`result.utterances[]`,已确定不再改写的片段,
///   逐词时间戳一并带出,喂给下游 diarization 按词切分)。
///
/// 厂商未提供语种标签,`lang` 恒为空串,交由调用方走文本兜底判断语言。
pub fn utterances_from_response(
    json: &serde_json::Value,
) -> (Option<String>, Vec<DefiniteUtterance>) {
    let result = &json["result"];
    let interim = result["text"].as_str().map(|s| s.to_string());

    let mut defs = Vec::new();
    if let Some(utterances) = result["utterances"].as_array() {
        for u in utterances {
            if u["definite"].as_bool() != Some(true) {
                continue;
            }
            let text = u["text"].as_str().unwrap_or_default().to_string();
            let start_ms = u["start_time"].as_u64().unwrap_or(0);
            let end_ms = u["end_time"].as_u64().unwrap_or(0);
            let words = u["words"]
                .as_array()
                .map(|ws| {
                    ws.iter()
                        .map(|w| CloudWord {
                            text: w["text"].as_str().unwrap_or_default().to_string(),
                            start_ms: w["start_time"].as_u64().unwrap_or(0),
                            end_ms: w["end_time"].as_u64().unwrap_or(0),
                        })
                        .collect()
                })
                .unwrap_or_default();
            defs.push(DefiniteUtterance {
                text,
                start_ms,
                end_ms,
                words,
                lang: String::new(),
            });
        }
    }

    (interim, defs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 测试助手:gzip 压缩字节(与生产端 full_request_frame 的调用方职责对称,
    /// 这里反过来构造"服务端已 gzip"的 golden 数据)。
    fn gzip(bytes: Vec<u8>) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn audio_frame_header_layout_matches_v3_protocol() {
        let f = audio_frame(&[0xAB, 0xCD], false);
        assert_eq!(
            &f[..4],
            &[0x11, 0x20, 0x00, 0x00],
            "ver1|hdr1, audio|no-seq, raw|none"
        );
        assert_eq!(&f[4..8], &2u32.to_be_bytes(), "大端长度");
        assert_eq!(&f[8..], &[0xAB, 0xCD]);
        let last = audio_frame(&[0x01], true);
        assert_eq!(last[1], 0x22, "末包 flags=0b0010");
    }

    #[test]
    fn parse_server_response_with_gzip_and_definite_utterance() {
        let json = serde_json::json!({"result":{"text":"你好","utterances":[
            {"text":"你好","start_time":0,"end_time":700,"definite":true,
             "words":[{"text":"你","start_time":0,"end_time":300}]}]}});
        let payload = gzip(serde_json::to_vec(&json).unwrap()); // 测试助手
        let mut buf = vec![0x11, 0x90, 0x11, 0x00]; // full response, JSON|gzip
        buf.extend_from_slice(&1i32.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        let ServerFrame::Response { seq, json } = parse_server_frame(&buf).unwrap() else {
            panic!()
        };
        assert_eq!(seq, 1);
        let (interim, defs) = utterances_from_response(&json);
        assert_eq!(interim.as_deref(), Some("你好"));
        assert_eq!(defs[0].end_ms, 700);
        assert_eq!(defs[0].words[0].text, "你");
    }

    #[test]
    fn parse_error_frame_yields_code_and_message() {
        let msg = "quota".as_bytes();
        let mut buf = vec![0x11, 0xF0, 0x00, 0x00];
        buf.extend_from_slice(&55000031u32.to_be_bytes());
        buf.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        buf.extend_from_slice(msg);
        let ServerFrame::Error { code, msg } = parse_server_frame(&buf).unwrap() else {
            panic!()
        };
        assert_eq!(code, 55000031);
        assert_eq!(msg, "quota");
    }

    #[test]
    fn parse_server_frame_rejects_truncated_and_unknown_frames() {
        // 与阿里侧 parse_event 的坏帧用例镜像:坏帧一律 Err,绝不 panic——收发循环
        // 只把 Err 当"丢弃继续收",一次下标越界 panic 会整条会话线程猝死。
        assert!(parse_server_frame(&[]).is_err(), "空帧");
        assert!(parse_server_frame(&[0x11, 0x90, 0x11]).is_err(), "不足 4 字节头部");
        // 完整响应帧:头部够了,但 seq+长度字段不全。
        assert!(parse_server_frame(&[0x11, 0x90, 0x11, 0x00, 0x00]).is_err(), "响应帧头后截断");
        // payload 长度声明越界(声明 999 字节,实际只有 2)。
        let mut buf = vec![0x11, 0x90, 0x11, 0x00];
        buf.extend_from_slice(&1i32.to_be_bytes());
        buf.extend_from_slice(&999u32.to_be_bytes());
        buf.extend_from_slice(&[0xAB, 0xCD]);
        assert!(parse_server_frame(&buf).is_err(), "payload 长度声明超出帧体");
        // 错误帧同款:消息长度声明越界。
        let mut buf = vec![0x11, 0xF0, 0x00, 0x00];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&999u32.to_be_bytes());
        buf.extend_from_slice(b"ab");
        assert!(parse_server_frame(&buf).is_err(), "错误消息长度声明超出帧体");
        // 未知消息类型(byte1 高4位 = 0b0101,协议里没有)。
        let mut buf = vec![0x11, 0x50, 0x00, 0x00];
        buf.extend_from_slice(&[0u8; 8]);
        assert!(parse_server_frame(&buf).is_err(), "未知消息类型");
        // 声明 gzip 但 payload 不是 gzip 流:解压失败也走 Err,不 panic。
        let mut buf = vec![0x11, 0x90, 0x11, 0x00];
        buf.extend_from_slice(&1i32.to_be_bytes());
        buf.extend_from_slice(&4u32.to_be_bytes());
        buf.extend_from_slice(b"junk");
        assert!(parse_server_frame(&buf).is_err(), "假 gzip payload");
    }
}
