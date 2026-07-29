//! 火山引擎(豆包)语音识别适配:WS 流式(sauc bigmodel)+ flash 批式补识。
//!
//! 分层:本文件只管"连接/线程/事件语义",所有协议字节一律经 `frames.rs` 的纯函数,
//! 这里不内联任何帧头拼装——协议正确性由 frames 的 golden 单测独占负责。
//!
//! 同步↔异步的桥:`CloudStream` 的 push/finish 是同步闭包(会话 worker 在自己的
//! crossbeam select 循环里调),而 WS 收发要 async。做法是每条流起一个专用线程跑
//! current_thread runtime,闭包侧只往 `tokio::sync::mpsc` 塞控制消息,不做任何
//! 会阻塞 worker 主循环的事(见 push 处的积压说明)。
//!
//! 关闭语义是与会话 worker 的硬约定(worker 据此决定要不要重连+补识):
//! - `Closed{error: None}` 只在"我方 finish 发出末包之后"出现;
//! - 其它任何终止(服务端主动关、错误帧、IO 错、上游未 finish 就丢流)都必须带
//!   `Some(原因)`,否则断连期间的音频会一直没人识别、直到停录才被发现。

pub mod frames;

use crate::asr::cloud::{f32_to_pcm_s16le, CloudAsr, CloudEvent, CloudStream, DefiniteUtterance};
use anyhow::{anyhow, bail, Context};
use frames::{
    audio_frame, full_request_frame, full_request_json, parse_server_frame,
    utterances_from_response, ServerFrame, FLASH_URL, RESOURCE_ID_FLASH, RESOURCE_ID_STREAM,
    WS_URL,
};
use futures_util::{SinkExt, StreamExt};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// 握手(TCP+TLS+WS upgrade+首帧配置)总上限。超时按"没开起来"处理,返回 Err,
/// 由 worker 的退避重连接手——绝不能挂死在这里,worker 的主循环在等我们返回。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 末包发出后等服务端吐完收尾定稿的上限。厂商通常吐完即关连接,这只是防挂死的闸。
const FINAL_DRAIN: Duration = Duration::from_secs(5);
/// 待发队列积压上限(条)。一条 ≈ 一帧音频(worker 帧率即节奏,约 100ms),
/// 64 条 ≈ 6s:超过说明连接实际已卡死,此时 push 报错让 worker 按断连处理
/// (记缺口 → 重连 → 批式补识),比无限涨内存或阻塞主循环都好。
const QUEUE_CAP: usize = 64;
/// flash 批式请求超时:调用方按 ≤15s 切段,单请求给足余量但不无限等。
const FLASH_TIMEOUT_S: u64 = 30;

/// 火山 flash 成功状态码(响应头 `X-Api-Status-Code`)。非此值代表业务失败,
/// 但 HTTP 仍是 200——不显式检查会把"配额耗尽"误读成"这段没人说话"。
const FLASH_STATUS_OK: &str = "20000000";

pub struct VolcanoAsr {
    app_key: String,
    access_key: String,
}

impl VolcanoAsr {
    pub fn new(app_key: String, access_key: String) -> Self {
        Self {
            app_key,
            access_key,
        }
    }
}

/// 送进 WS 线程的控制消息。音频与末包走同一条队列是刻意的:保序。末包若走旁路
/// (另一个 channel),select 的随机性会让它插到尾部音频前面,直接截掉句尾。
enum Ctl {
    Audio(Vec<u8>),
    Finish,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl CloudAsr for VolcanoAsr {
    /// 同步语义:返回 Ok 即代表"连接已建立、配置帧已发出",可以立刻推音频。
    /// worker 拿到 Ok 的那一刻就会闭合缺口(session.rs `try_recover`),所以握手
    /// 绝不能异步化成"先给流、失败再报 Closed"——那样握手期间推进来的音频既没送到
    /// 厂商、也不在缺口里,会被静默吞掉。
    fn open_stream(&self) -> anyhow::Result<CloudStream> {
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Ctl>();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<CloudEvent>();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);
        let pending = Arc::new(AtomicUsize::new(0));

        let app_key = self.app_key.clone();
        let access_key = self.access_key.clone();
        let thread_pending = pending.clone();
        std::thread::Builder::new()
            .name("volcano-ws".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("建 tokio runtime 失败: {e}")));
                        return;
                    }
                };
                rt.block_on(async move {
                    let ws = match tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        handshake(&app_key, &access_key),
                    )
                    .await
                    {
                        Err(_) => {
                            let _ = ready_tx
                                .send(Err(format!("握手超时(>{}s)", CONNECT_TIMEOUT.as_secs())));
                            return;
                        }
                        Ok(Err(e)) => {
                            let _ = ready_tx.send(Err(format!("握手失败: {e:#}")));
                            return;
                        }
                        Ok(Ok(ws)) => ws,
                    };
                    // 握手成功先放行 open_stream,再进收发循环:worker 早一步拿到流,
                    // 也保证 ready 通道只被 send 一次。
                    let _ = ready_tx.send(Ok(()));
                    run_session(ws, ctl_rx, ev_tx, thread_pending).await;
                });
            })
            .context("起火山 WS 线程失败")?;

        // 线程内已有 CONNECT_TIMEOUT 闸,这里再多给一点余量兜住线程调度抖动;
        // recv_timeout 本身也不会永久阻塞 worker。
        match ready_rx.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bail!("火山流未能建立: {e}"),
            Err(e) => bail!("火山流握手无响应: {e}"),
        }

        let audio_tx = ctl_tx.clone();
        let finish_tx = ctl_tx;
        Ok(CloudStream {
            push: Box::new(move |samples: &[f32]| {
                if samples.is_empty() {
                    return Ok(());
                }
                if pending.load(Ordering::Relaxed) >= QUEUE_CAP {
                    bail!("火山发送队列积压 ≥{QUEUE_CAP} 帧,按断连处理");
                }
                // f32→PCM 在调用方线程做:转换是纯算术(比 memcpy 贵不了多少),
                // 换来跨线程只搬一份紧凑字节,也让 WS 线程只管协议。
                let pcm = f32_to_pcm_s16le(samples);
                pending.fetch_add(1, Ordering::Relaxed);
                audio_tx
                    .send(Ctl::Audio(pcm))
                    .map_err(|_| anyhow!("火山流已关闭,推流失败"))
            }),
            finish: Box::new(move || {
                // 只入队,不等排干:worker 是逐源串行调 finish 的,在这里阻塞会把
                // 各源的收尾时延串起来(停录卡顿)。末包发送与排干由 WS 线程按序
                // 完成,并在 FINAL_DRAIN 内必定发出 Closed;worker 自己的排干窗口
                // (CLOUD_DRAIN_MS)才是真正的等待区。
                finish_tx
                    .send(Ctl::Finish)
                    .map_err(|_| anyhow!("火山流已关闭,末包未发出"))
            }),
            events: ev_rx,
        })
    }

    fn transcribe_batch(&self, samples: &[f32]) -> anyhow::Result<Vec<DefiniteUtterance>> {
        if samples.is_empty() {
            // 本地拦截:空段没有可识别内容,发出去只会白烧一次配额和一个 RTT。
            bail!("空音频不发批式请求");
        }
        let wav = wav_bytes(samples)?;
        let body = serde_json::json!({
            "user": {"uid": "voice-notes"},
            "audio": {
                "format": "wav",
                "data": base64_std(&wav),
            },
            "request": {
                "model_name": "bigmodel",
                "enable_itn": true,
                "enable_punc": true,
                "show_utterances": true,
            },
        });

        let resp = ureq::post(FLASH_URL)
            .timeout(Duration::from_secs(FLASH_TIMEOUT_S))
            .set("X-Api-App-Key", &self.app_key)
            .set("X-Api-Access-Key", &self.access_key)
            .set("X-Api-Resource-Id", RESOURCE_ID_FLASH)
            .set("X-Api-Request-Id", &uuid::Uuid::new_v4().to_string())
            .set("X-Api-Sequence", "-1")
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .context("火山 flash 请求失败")?;

        // 业务码在头里,HTTP 恒 200:先验码再解 JSON,否则失败会被当成静音。
        if let Some(code) = resp.header("X-Api-Status-Code") {
            if code != FLASH_STATUS_OK {
                let msg = resp.header("X-Api-Message").unwrap_or_default().to_string();
                bail!("火山 flash 返回业务错误 {code}: {msg}");
            }
        }
        let text = resp.into_string().context("读火山 flash 响应体失败")?;
        let json: serde_json::Value =
            serde_json::from_str(&text).context("解析火山 flash 响应 JSON 失败")?;

        let (interim, defs) = utterances_from_response(&json);
        if !defs.is_empty() {
            return Ok(defs);
        }
        // 兜底:批式接口的 utterances 未必带 definite 标记(流式才需要"还会改写"这个
        // 概念),这时整段文本本身就是定稿。退化成一条覆盖 [0, 段长] 的 utterance,
        // 与阿里批式的偏差处理同形(spec §2.1),调用方叠加段偏移即可。
        let text = interim.unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![DefiniteUtterance {
            text,
            start_ms: 0,
            end_ms: (samples.len() as u64) * 1000 / 16000,
            words: Vec::new(),
            lang: String::new(),
        }])
    }
}

/// 建连 + 发首帧配置。两步合成"握手"是有意的:配置帧发不出去的连接对上层毫无
/// 用处,不如在 open_stream 就报错,让 worker 走退避,而不是先给一条注定要死的流。
async fn handshake(app_key: &str, access_key: &str) -> anyhow::Result<WsStream> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut req = WS_URL
        .into_client_request()
        .context("构造 WS 握手请求失败")?;
    let headers = req.headers_mut();
    headers.insert("X-Api-App-Key", HeaderValue::from_str(app_key)?);
    headers.insert("X-Api-Access-Key", HeaderValue::from_str(access_key)?);
    headers.insert(
        "X-Api-Resource-Id",
        HeaderValue::from_static(RESOURCE_ID_STREAM),
    );
    headers.insert(
        "X-Api-Request-Id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())?,
    );
    // -1 = 本连接只有一个会话,不做会话内分片续传。
    headers.insert("X-Api-Sequence", HeaderValue::from_static("-1"));

    let (mut ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .context("WS 建连失败")?;

    // 热词暂不接入(留给设置面板打通后再传 Some);None 走无 corpus 的默认配置。
    let payload = gzip(&serde_json::to_vec(&full_request_json(None))?)?;
    ws.send(Message::binary(full_request_frame(&payload)))
        .await
        .context("发送配置帧失败")?;
    Ok(ws)
}

/// 收发主循环。任何出口都恰好发一次 Closed:None 只属于"我方 finish"这一条路径。
async fn run_session(
    ws: WsStream,
    mut ctl_rx: tokio::sync::mpsc::UnboundedReceiver<Ctl>,
    ev_tx: crossbeam_channel::Sender<CloudEvent>,
    pending: Arc<AtomicUsize>,
) {
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            ctl = ctl_rx.recv() => match ctl {
                Some(Ctl::Audio(pcm)) => {
                    pending.fetch_sub(1, Ordering::Relaxed);
                    if let Err(e) = sink.send(Message::binary(audio_frame(&pcm, false))).await {
                        let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("音频帧发送失败: {e}")) });
                        return;
                    }
                }
                Some(Ctl::Finish) => {
                    finish_and_drain(sink, stream, &ev_tx).await;
                    return;
                }
                // 两个发送端都没了 = CloudStream 整个被丢弃却没调 finish(worker 侧
                // 只在断连重连时这么干)。末包没发,厂商侧结果不完整 → 必须报 Some。
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    let _ = ev_tx.send(CloudEvent::Closed { error: Some("上游未 finish 即丢弃流".into()) });
                    return;
                }
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    match parse_server_frame(&bytes) {
                        Ok(ServerFrame::Response { json, .. }) => {
                            if !emit_response(&json, &ev_tx) {
                                return; // 上游不再收事件:收摊,别把线程和连接漏着
                            }
                        }
                        Ok(ServerFrame::Error { code, msg }) => {
                            let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("厂商错误帧 {code}: {msg}")) });
                            return;
                        }
                        // 坏帧不致命(可能是没见过的消息类型),丢弃继续收。
                        Err(e) => eprintln!("火山下行帧解析失败: {e:#}"),
                    }
                }
                // 服务端主动关闭:哪怕是干净的 close 帧,对我们也是"没说完就断了"。
                Some(Ok(Message::Close(f))) => {
                    let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("服务端主动关闭: {f:?}")) });
                    return;
                }
                // Ping/Pong/Text:tungstenite 自动回 Pong,业务上无内容,忽略。
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("WS 读错误: {e}")) });
                    return;
                }
                None => {
                    let _ = ev_tx.send(CloudEvent::Closed { error: Some("连接被对端关闭".into()) });
                    return;
                }
            },
        }
    }
}

/// 末包 + 有界排干。排干整体套一层 timeout:厂商吐完收尾结果通常随即关连接,
/// 但不能把"它不关"变成我们挂死。
async fn finish_and_drain(
    mut sink: futures_util::stream::SplitSink<WsStream, Message>,
    mut stream: futures_util::stream::SplitStream<WsStream>,
    ev_tx: &crossbeam_channel::Sender<CloudEvent>,
) {
    if let Err(e) = sink.send(Message::binary(audio_frame(&[], true))).await {
        let _ = ev_tx.send(CloudEvent::Closed {
            error: Some(format!("末包发送失败: {e}")),
        });
        return;
    }

    let drained = tokio::time::timeout(FINAL_DRAIN, async {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(Message::Binary(bytes)) => match parse_server_frame(&bytes) {
                    Ok(ServerFrame::Response { json, .. }) => {
                        if !emit_response(&json, ev_tx) {
                            return None;
                        }
                    }
                    Ok(ServerFrame::Error { code, msg }) => {
                        return Some(format!("厂商错误帧 {code}: {msg}"));
                    }
                    Err(e) => eprintln!("火山收尾帧解析失败: {e:#}"),
                },
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    })
    .await;

    let _ = sink.send(Message::Close(None)).await;
    // 末包已发出:超时/对端关闭/读错都只是收尾过程的结束方式,没有重连语义 → None。
    // 只有厂商明确回错误帧才带上原因(录制已停,worker 排干阶段不区分,留作日志)。
    let error = drained.unwrap_or(None);
    let _ = ev_tx.send(CloudEvent::Closed { error });
}

/// 把一条服务端响应摊成事件。返回 false 表示上游已不再接收(通道断开)。
fn emit_response(json: &serde_json::Value, ev_tx: &crossbeam_channel::Sender<CloudEvent>) -> bool {
    let (interim, defs) = utterances_from_response(json);
    if let Some(text) = interim {
        if !text.is_empty() && ev_tx.send(CloudEvent::Interim { text }).is_err() {
            return false;
        }
    }
    for d in defs {
        if ev_tx.send(CloudEvent::Definite(d)).is_err() {
            return false;
        }
    }
    true
}

/// f32 → 内存 WAV(16k 单声道 s16le)。批式接口只吃带头的容器格式,而流式吃裸 PCM;
/// 采样值转换复用 `f32_to_pcm_s16le`,保证两条路的钳制规则不漂移。
fn wav_bytes(samples: &[f32]) -> anyhow::Result<Vec<u8>> {
    let pcm = f32_to_pcm_s16le(samples);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).context("写 WAV 头失败")?;
        for frame in pcm.chunks_exact(2) {
            writer.write_sample(i16::from_le_bytes([frame[0], frame[1]]))?;
        }
        writer.finalize().context("收尾 WAV 失败")?;
    }
    Ok(cursor.into_inner())
}

fn base64_std(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn gzip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_rejects_empty_samples_without_network() {
        let asr = VolcanoAsr::new("app".into(), "key".into());
        // 空音频本地拦截:不发请求(测试机无凭证也无网,能跑通即证明没走网络)。
        assert!(asr.transcribe_batch(&[]).is_err());
    }

    #[test]
    fn wav_bytes_wraps_pcm_with_canonical_44_byte_header() {
        let wav = wav_bytes(&[0.0f32; 100]).unwrap();
        assert_eq!(wav.len(), 44 + 2 * 100, "s16 单声道 WAV = 44 字节头 + 2n");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    #[test]
    fn gzip_payload_roundtrips_through_frame_parser_expectations() {
        // 配置帧的 payload 必须是合法 gzip:frames 侧解码按 gzip 处理,这里守住写侧。
        let raw = serde_json::to_vec(&full_request_json(None)).unwrap();
        let z = gzip(&raw).unwrap();
        assert_eq!(&z[0..2], &[0x1f, 0x8b], "gzip 魔数");
        assert!(z.len() < raw.len() + 32);
    }
}
