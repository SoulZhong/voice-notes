//! 阿里云 DashScope 语音识别协议(纯函数,不碰网络)。
//!
//! 协议要点(spec §2.1 阿里节,2026-07 官方文档核实):
//! - 流式走 WebSocket 的"任务"协议:**文本帧发 JSON 指令、二进制帧发裸 PCM**。
//!   与火山"全部二进制自定义帧头"完全不同,所以这里没有帧头拼装,只有 JSON 构造/解析。
//! - 一条连接 = 一个 task,由客户端生成 `task_id` 串起 run-task / finish-task /
//!   下行事件三方。`streaming: "duplex"` 表示上行音频与下行结果同时进行。
//! - 下行事件全在 `header.event`:task-started / result-generated / task-finished /
//!   task-failed(后者把原因放 `header.error_code` + `header.error_message`)。
//! - 识别结果是"一句一状态机":同一句会被反复吐出(文本越来越长),
//!   `sentence_end=false` 是中间态、`true` 才是定稿;`heartbeat=true` 的句子是
//!   保活占位(无实际内容),必须丢弃,否则会把空串当成一次识别结果上报。
//! - 时间戳单位 ms,`end_time` 在句子定稿前为 null。

use crate::asr::cloud::CloudWord;
use anyhow::{anyhow, bail, Context};

/// 流式识别 WS 端点(路径末尾的斜杠是官方写法,别省)。
pub const WS_URL: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference/";
/// 实时模型:中文效果与延迟的官方推荐档,带逐词时间戳。
pub const REALTIME_MODEL: &str = "fun-asr-realtime";
/// 批式(补识)端点:多模态生成接口,音频以 data URI 内联。
pub const BATCH_URL: &str =
    "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation";
/// 批式模型。注意它**不返回时间戳**(spec §2.1 已记该偏差),
/// 所以补识只能给出一条覆盖整段的 utterance。
pub const BATCH_MODEL: &str = "qwen3-asr-flash";

/// 开任务指令(文本帧)。发出后必须等 `task-started` 才能推音频——服务端在
/// task-started 之前收到二进制帧会直接判协议错。
///
/// `heartbeat: true` 是刻意打开的:默认行为下,持续上行**静音**会被服务端判为
/// 无有效音频并在静默约 60s 后掐断会话。我们的采集是一直推流的(会议里的长时间
/// 沉默照样上行静音样本),正好落在这个雷区里,所以必须开。代价是下行多了一类
/// `heartbeat=true` 的保活占位句,解析侧必须显式丢弃(见适配层 `emit_sentence`)。
pub fn run_task_msg(task_id: &str) -> serde_json::Value {
    serde_json::json!({
        "header": {
            "action": "run-task",
            "task_id": task_id,
            "streaming": "duplex",
        },
        "payload": {
            "task_group": "audio",
            "task": "asr",
            "function": "recognition",
            "model": REALTIME_MODEL,
            "parameters": {
                // 裸 PCM 16k 单声道 s16le:与 f32_to_pcm_s16le 的输出一致,
                // 上行不做任何容器封装。
                "format": "pcm",
                "sample_rate": 16000,
                "heartbeat": true,
            },
            "input": {},
        },
    })
}

/// 末包指令(文本帧)。服务端收到后把缓冲里的尾音识别完、吐完最后的定稿,
/// 再回 `task-finished`。`task_id` 必须与 run-task 一致,否则服务端不认。
pub fn finish_task_msg(task_id: &str) -> serde_json::Value {
    serde_json::json!({
        "header": {
            "action": "finish-task",
            "task_id": task_id,
            "streaming": "duplex",
        },
        "payload": { "input": {} },
    })
}

/// 下行事件的语义化形态。适配层只按这个枚举做状态迁移,不再碰原始 JSON。
#[derive(Debug, Clone, PartialEq)]
pub enum AliEvent {
    Started,
    Sentence {
        text: String,
        begin_ms: u64,
        /// 句子定稿前为 None(服务端给 null)。
        end_ms: Option<u64>,
        sentence_end: bool,
        words: Vec<CloudWord>,
        /// true = 保活占位句,无内容,调用方必须丢弃。
        heartbeat: bool,
    },
    Finished,
    Failed {
        code: String,
        msg: String,
    },
}

/// 解析一个下行文本帧。返回 Err 表示"这帧不是我们认识的事件"——适配层按可忽略
/// 处理(记日志继续收),而不是当成连接故障:厂商随时可能加新事件类型。
pub fn parse_event(text_frame: &str) -> anyhow::Result<AliEvent> {
    let v: serde_json::Value =
        serde_json::from_str(text_frame).context("解析 DashScope 事件 JSON 失败")?;
    let event = v["header"]["event"]
        .as_str()
        .ok_or_else(|| anyhow!("下行帧缺 header.event"))?;
    match event {
        "task-started" => Ok(AliEvent::Started),
        "task-finished" => Ok(AliEvent::Finished),
        "task-failed" => Ok(AliEvent::Failed {
            // 兜底成占位串而不是报错:错误事件本身是终止信号,哪怕字段缺了也
            // 必须把"失败"这个语义送上去,否则会被降级成可忽略的坏帧。
            code: v["header"]["error_code"]
                .as_str()
                .unwrap_or("UNKNOWN")
                .to_string(),
            msg: v["header"]["error_message"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        }),
        "result-generated" => {
            let s = &v["payload"]["output"]["sentence"];
            if !s.is_object() {
                bail!("result-generated 缺 payload.output.sentence");
            }
            Ok(AliEvent::Sentence {
                text: s["text"].as_str().unwrap_or_default().to_string(),
                begin_ms: s["begin_time"].as_u64().unwrap_or(0),
                end_ms: s["end_time"].as_u64(),
                sentence_end: s["sentence_end"].as_bool().unwrap_or(false),
                words: words_from(&s["words"]),
                heartbeat: s["heartbeat"].as_bool().unwrap_or(false),
            })
        }
        other => bail!("未知下行事件: {other}"),
    }
}

/// 逐词表映射。词文本要**拼上 punctuation**:阿里把标点单独挂在词上,
/// 拼接后的词序列才等于整句文本。下游 diarization 按词切分后是用词表重组说话人
/// 分句的(session.rs `utterance_to_transcript` → tokens),丢掉标点等于让分说话人
/// 之后的文本比原句少标点。
fn words_from(v: &serde_json::Value) -> Vec<CloudWord> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .map(|w| {
            let mut text = w["text"].as_str().unwrap_or_default().to_string();
            text.push_str(w["punctuation"].as_str().unwrap_or_default());
            let start_ms = w["begin_time"].as_u64().unwrap_or(0);
            CloudWord {
                text,
                start_ms,
                // 词级 end_time 理论上总有;缺失时退化成零长词,保证时间轴单调。
                end_ms: w["end_time"].as_u64().unwrap_or(start_ms),
            }
        })
        .collect()
}

/// 批式请求体。音频以 `data:audio/wav;base64,…` 内联(≤10MB / ≤5min,
/// 调用方按 ≤15s 切段,余量充足)。`enable_itn` 打开逆文本正则化,
/// 让数字/日期以书面形式落地,与流式档的默认行为对齐。
pub fn batch_request_json(wav_base64: &str) -> serde_json::Value {
    serde_json::json!({
        "model": BATCH_MODEL,
        "input": {
            "messages": [{
                "role": "user",
                "content": [{ "audio": format!("data:audio/wav;base64,{wav_base64}") }],
            }],
        },
        "parameters": { "asr_options": { "enable_itn": true } },
    })
}

/// 从批式响应里取出识别文本。
///
/// 返回 Err 只代表"响应形状不符"(路径缺失/类型不对),**不代表没人说话**;
/// `Ok("")` 才是厂商明确表示这段是静音。两者绝不能折叠:把形状错误当静音,
/// 会让一整段音频被判定为无人说话而永久丢失。
pub fn batch_text_from_response(json: &serde_json::Value) -> anyhow::Result<String> {
    let content = &json["output"]["choices"][0]["message"]["content"];
    // content 官方是数组,元素形如 {"text": "..."};但历史/降级响应里也出现过
    // 直接给字符串的形态,两种都收下,以免因为一层包装把结果判成形状错误。
    if let Some(s) = content.as_str() {
        return Ok(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(s) = item["text"].as_str() {
                return Ok(s.to_string());
            }
        }
    }
    bail!("批式响应缺 output.choices[0].message.content[].text")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_task_msg_carries_duplex_funasr_and_heartbeat() {
        let v = run_task_msg("tid");
        assert_eq!(v["header"]["action"], "run-task");
        assert_eq!(v["header"]["streaming"], "duplex");
        assert_eq!(v["header"]["task_id"], "tid");
        assert_eq!(v["payload"]["model"], "fun-asr-realtime");
        assert_eq!(v["payload"]["parameters"]["format"], "pcm");
        assert_eq!(v["payload"]["parameters"]["sample_rate"], 16000);
        assert_eq!(
            v["payload"]["parameters"]["heartbeat"], true,
            "防 60s 静默断连(spec §2.1)"
        );
    }

    #[test]
    fn finish_task_msg_reuses_task_id_and_duplex() {
        let v = finish_task_msg("tid");
        assert_eq!(v["header"]["action"], "finish-task");
        assert_eq!(v["header"]["task_id"], "tid", "末包必须挂在同一个 task 上");
        assert_eq!(v["header"]["streaming"], "duplex");
    }

    #[test]
    fn parse_event_definite_sentence_with_words() {
        let raw = r#"{"header":{"task_id":"t","event":"result-generated","attributes":{}},
          "payload":{"output":{"sentence":{"begin_time":170,"end_time":1500,"text":"好的","heartbeat":false,
          "sentence_end":true,"words":[{"begin_time":170,"end_time":300,"text":"好","punctuation":"，"}]}},
          "usage":{"duration":3}}}"#;
        let AliEvent::Sentence {
            text,
            begin_ms,
            end_ms,
            sentence_end,
            words,
            heartbeat,
        } = parse_event(raw).unwrap()
        else {
            panic!()
        };
        assert!(sentence_end && !heartbeat);
        assert_eq!((text.as_str(), begin_ms, end_ms), ("好的", 170, Some(1500)));
        // 偏离 brief 的字面断言("好"):协议参考要求词文本拼上 punctuation,
        // 否则词序列重组不回整句(见 words_from 的说明)。
        assert_eq!(words[0].text, "好，");
        assert_eq!((words[0].start_ms, words[0].end_ms), (170, 300));
    }

    #[test]
    fn parse_event_interim_sentence_has_no_end_time() {
        let raw = r#"{"header":{"event":"result-generated"},
          "payload":{"output":{"sentence":{"begin_time":0,"end_time":null,"text":"你","sentence_end":false,"words":[]}}}}"#;
        let AliEvent::Sentence {
            end_ms,
            sentence_end,
            heartbeat,
            ..
        } = parse_event(raw).unwrap()
        else {
            panic!()
        };
        assert_eq!(end_ms, None, "定稿前 end_time 为 null");
        assert!(!sentence_end && !heartbeat, "缺字段按 false 处理");
    }

    #[test]
    fn parse_event_marks_heartbeat_sentence() {
        let raw = r#"{"header":{"event":"result-generated"},
          "payload":{"output":{"sentence":{"begin_time":0,"text":"","heartbeat":true,"sentence_end":false,"words":[]}}}}"#;
        let AliEvent::Sentence { heartbeat, .. } = parse_event(raw).unwrap() else {
            panic!()
        };
        assert!(heartbeat, "保活占位句必须被标出来,好让适配层丢弃");
    }

    #[test]
    fn parse_event_task_failed_carries_code() {
        let raw = r#"{"header":{"task_id":"t","event":"task-failed","error_code":"CLIENT_ERROR","error_message":"timeout","attributes":{}},"payload":{}}"#;
        let AliEvent::Failed { code, msg } = parse_event(raw).unwrap() else {
            panic!()
        };
        assert_eq!((code.as_str(), msg.as_str()), ("CLIENT_ERROR", "timeout"));
    }

    #[test]
    fn parse_event_started_and_finished() {
        assert_eq!(
            parse_event(r#"{"header":{"event":"task-started"},"payload":{}}"#).unwrap(),
            AliEvent::Started
        );
        assert_eq!(
            parse_event(r#"{"header":{"event":"task-finished"},"payload":{}}"#).unwrap(),
            AliEvent::Finished
        );
    }

    #[test]
    fn parse_event_rejects_unknown_and_malformed_frames() {
        assert!(parse_event("not json").is_err());
        assert!(parse_event(r#"{"header":{}}"#).is_err(), "缺 event");
        assert!(parse_event(r#"{"header":{"event":"task-unknown"}}"#).is_err());
        assert!(
            parse_event(r#"{"header":{"event":"result-generated"},"payload":{}}"#).is_err(),
            "缺 sentence 的结果帧不能被当成空句上报"
        );
    }

    #[test]
    fn batch_request_json_inlines_wav_data_uri() {
        let v = batch_request_json("QUJD");
        assert_eq!(v["model"], "qwen3-asr-flash");
        assert_eq!(
            v["input"]["messages"][0]["content"][0]["audio"],
            "data:audio/wav;base64,QUJD"
        );
        assert_eq!(v["parameters"]["asr_options"]["enable_itn"], true);
    }

    #[test]
    fn batch_text_from_response_reads_first_text_item() {
        let json = serde_json::json!({"output":{"choices":[{"message":{"content":[
            {"other":"x"}, {"text":"你好世界"}]}}]}});
        assert_eq!(batch_text_from_response(&json).unwrap(), "你好世界");
    }

    #[test]
    fn batch_text_from_response_tolerates_plain_string_content() {
        let json = serde_json::json!({"output":{"choices":[{"message":{"content":"你好"}}]}});
        assert_eq!(batch_text_from_response(&json).unwrap(), "你好");
    }

    #[test]
    fn batch_text_from_response_distinguishes_silence_from_bad_shape() {
        let silent = serde_json::json!({"output":{"choices":[{"message":{"content":[{"text":""}]}}]}});
        assert_eq!(batch_text_from_response(&silent).unwrap(), "", "空串 = 静音");
        let err = serde_json::json!({"code":"InvalidApiKey","message":"bad key"});
        assert!(
            batch_text_from_response(&err).is_err(),
            "错误响应不能被读成静音"
        );
    }
}
