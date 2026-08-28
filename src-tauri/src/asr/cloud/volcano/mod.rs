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

use crate::asr::cloud::{
    f32_to_pcm_s16le, CloudAsr, CloudEvent, CloudStream, DefiniteUtterance, CLOUD_DRAIN_MS,
};
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
/// 预算必须短:open_stream 是同步的、阻塞 worker 主循环(期间所有源都不推进),
/// 断网时靠 worker 的退避机制继续重试,不靠这里死等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 末包发出后等服务端吐完收尾定稿的上限。厂商通常吐完即关连接,这只是防挂死的闸。
/// 必须小于 worker 排干窗口,否则 (worker, adapter] 区间的尾段定稿被丢:我们还在
/// 收,worker 已经不听了。故直接由 CLOUD_DRAIN_MS 收窄 500ms 派生,不写死第二个值。
const FINAL_DRAIN: Duration = Duration::from_millis(CLOUD_DRAIN_MS.saturating_sub(500));
/// 待发积压上限(样本数,16k 单声道)。设备回调链上来的一次 push 只有 10–21ms,
/// 按条数计根本量不出时间,所以按样本算:16000×5 = 5s 音频。超过说明连接事实上
/// 已不可用(TCP 发不出去/服务端不收),此时 push 报错让 worker 按断连处理
/// (记缺口 → 重连 → 批式补识),比无限涨内存或阻塞主循环都好。
const QUEUE_CAP_SAMPLES: usize = 16000 * 5;
/// 上行分包粒度(字节):3200 = 100ms @ 16k/s16le。火山建议 100–200ms 一包;
/// 逐次 push(10–21ms)直发会把帧数放大近 10 倍,徒增帧头开销与服务端调度压力。
const UPLINK_CHUNK_BYTES: usize = 3200;
/// 空闲保活:距上一次**真正发上线**的音频帧超过这么久,就补一帧静音。
///
/// 为什么需要:重连之后会话 worker 会在自己的循环里**同步**跑缺口补识
/// (最多约 20 次 transcribe_batch,40–80s),这段时间它根本回不到 select 循环、
/// 一帧音频都不会 push,新开的流上是**零上行帧**。火山对此的约束是等包超时
/// (错误 45000081,与阿里 23s 无音频即 task-failed 同类)。
/// 配置帧里的静音/心跳相关参数只覆盖"有帧但内容是静音",覆盖不了"零帧"。
///
/// 分工(与 session.rs CLOUD_IDLE_CLOSE_MS 注释对称):本保活只负责"worker 被同步
/// 补识阻塞"这类**有界**窗口(≤80s → 注入 ≤0.8s 静音,时钟偏移已在下方分支算过账);
/// **暂停**那种无界静默由 worker 侧的闲置关流兜底——它的门槛(8s)严格小于这里的
/// 10s,所以暂停永远轮不到保活开火。改这两个值时保持这个大小关系(session.rs
/// 有一条常数关系测试盯着)。
pub(crate) const KEEPALIVE_IDLE_MS: u64 = 10_000;
/// 保活检查节拍。粒度要远小于 KEEPALIVE_IDLE_MS,让实际静默上限 ≈ idle + 一个节拍,
/// 与厂商的等包超时之间留足余量。
const KEEPALIVE_TICK: Duration = Duration::from_secs(2);
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

        // 线程内已有 CONNECT_TIMEOUT 闸,这里再多给 1s 余量兜住线程调度抖动;
        // recv_timeout 本身也不会永久阻塞 worker。同样按"短预算"原则:这条 recv
        // 就是 worker 主循环的停顿时长。
        match ready_rx.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(1)) {
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
                if pending.load(Ordering::Relaxed) >= QUEUE_CAP_SAMPLES {
                    bail!(
                        "火山待发音频积压 ≥{}s,按断连处理",
                        QUEUE_CAP_SAMPLES / 16000
                    );
                }
                // f32→PCM 在调用方线程做:转换是纯算术(比 memcpy 贵不了多少),
                // 换来跨线程只搬一份紧凑字节,也让 WS 线程只管协议。
                let pcm = f32_to_pcm_s16le(samples);
                let n = samples.len();
                pending.fetch_add(n, Ordering::Relaxed);
                audio_tx.send(Ctl::Audio(pcm)).map_err(|_| {
                    // 没进队列就不算积压:失败路径回滚计数,别让死流的残值污染语义。
                    pending.fetch_sub(n, Ordering::Relaxed);
                    anyhow!("火山流已关闭,推流失败")
                })
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

        let resp = match crate::netproxy::agent_for(&FLASH_URL).post(FLASH_URL)
            .timeout(Duration::from_secs(FLASH_TIMEOUT_S))
            .set("X-Api-App-Key", &self.app_key)
            .set("X-Api-Access-Key", &self.access_key)
            .set("X-Api-Resource-Id", RESOURCE_ID_FLASH)
            .set("X-Api-Request-Id", &uuid::Uuid::new_v4().to_string())
            .set("X-Api-Sequence", "-1")
            .set("content-type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(resp) => resp,
            // 非 2xx:火山把业务错误说明(鉴权/配额/参数)放在响应体里,只报状态码
            // 等于把唯一有用的信息扔掉。
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                bail!("火山 flash HTTP {code}: {}", head_of(&body));
            }
            Err(e) => return Err(anyhow::Error::new(e).context("火山 flash 请求失败")),
        };

        // 业务码在头里,HTTP 恒 200:先验码再解 JSON,否则失败会被当成静音。
        let status = resp.header("X-Api-Status-Code").map(|s| s.to_string());
        let api_msg = resp.header("X-Api-Message").unwrap_or_default().to_string();
        let text = resp.into_string().context("读火山 flash 响应体失败")?;
        match status.as_deref() {
            // 头都没有 = 响应根本不是我们认识的那个接口的形状(网关拦截页/重定向/
            // 改版)。这种情况下解出来的"没文本"绝不能当静音上报,否则整段音频
            // 被判定为无人说话而永久丢失。
            None => bail!("火山 flash 响应缺 X-Api-Status-Code 头: {}", head_of(&text)),
            Some(code) if code != FLASH_STATUS_OK => {
                bail!("火山 flash 返回业务错误 {code}: {api_msg}")
            }
            Some(_) => {}
        }
        let json: serde_json::Value =
            serde_json::from_str(&text).context("解析火山 flash 响应 JSON 失败")?;

        let (interim, defs) = utterances_from_response(&json);
        flash_utterances(interim, defs, (samples.len() as u64) * 1000 / 16000)
    }
}

/// flash 响应的"有话/没话/形状不对"三分决策(纯函数,便于单测)。
///
/// 关键区分:`interim: None` 表示 `result.text` 字段整个不存在——即响应形状不符,
/// 必须报错走上层重试/记缺口;`Some("")` 才是厂商明确表示"这段没人说话"。
/// 把前者折叠成后者,等于用一次静默的数据丢失换一次安静的日志。
fn flash_utterances(
    interim: Option<String>,
    defs: Vec<DefiniteUtterance>,
    samples_ms: u64,
) -> anyhow::Result<Vec<DefiniteUtterance>> {
    if !defs.is_empty() {
        return Ok(defs);
    }
    let Some(text) = interim else {
        bail!("火山 flash 响应无 result.text 字段且无分句,形状不符,不按静音处理");
    };
    // 兜底:批式接口的 utterances 未必带 definite 标记(流式才需要"还会改写"这个
    // 概念),这时整段文本本身就是定稿。退化成一条覆盖 [0, 段长] 的 utterance,
    // 与阿里批式的偏差处理同形(spec §2.1),调用方叠加段偏移即可。
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![DefiniteUtterance {
        text,
        start_ms: 0,
        end_ms: samples_ms,
        words: Vec::new(),
        lang: String::new(),
    }])
}

/// 报错时截取响应体开头,够定位问题又不至于把整页 HTML 灌进日志。按字符边界切,
/// 免得中文错误说明被切成非法 UTF-8。
fn head_of(body: &str) -> String {
    const MAX: usize = 200;
    match body.char_indices().nth(MAX) {
        None => body.to_string(),
        Some((i, _)) => format!("{}…", &body[..i]),
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
///
/// 关于"必须持续有音频帧上行"的服务端约束:正常录制时采集回调 10–21ms 一次,
/// 天然满足;但**重连之后**worker 会先同步跑完缺口补识(最多约 20 次
/// transcribe_batch,40–80s)才回到自己的 select 循环,期间这条新流上零上行帧,
/// 会撞上厂商的等包超时(45000081)。故此处有一条 KEEPALIVE_TICK 的定时分支补静音帧。
async fn run_session(
    ws: WsStream,
    mut ctl_rx: tokio::sync::mpsc::UnboundedReceiver<Ctl>,
    ev_tx: crossbeam_channel::Sender<CloudEvent>,
    pending: Arc<AtomicUsize>,
) {
    let (mut sink, mut stream) = ws.split();
    // 上行聚合缓冲:push 的粒度由设备回调链决定(10–21ms),这里攒够
    // UPLINK_CHUNK_BYTES 再发,避免一次 push 一帧的碎包。
    let mut uplink = Vec::with_capacity(UPLINK_CHUNK_BYTES * 2);
    // 保活以"上一次真正发出去的音频帧"为基准,而不是"上一次收到 push":攒在
    // uplink 里没发出的字节对服务端不存在。保活帧自己也刷新这个时间戳,所以它
    // 天然自限流(静默期每 KEEPALIVE_IDLE_MS 一帧),不会连发。
    let mut last_audio_sent = tokio::time::Instant::now();
    let mut keepalive = tokio::time::interval(KEEPALIVE_TICK);
    loop {
        tokio::select! {
            ctl = ctl_rx.recv() => match ctl {
                Some(Ctl::Audio(pcm)) => {
                    pending.fetch_sub(pcm.len() / 2, Ordering::Relaxed);
                    uplink.extend_from_slice(&pcm);
                    if uplink.len() >= UPLINK_CHUNK_BYTES {
                        let frame = audio_frame(&uplink, false);
                        uplink.clear();
                        if let Err(e) = sink.send(Message::binary(frame)).await {
                            let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("音频帧发送失败: {e}")) });
                            return;
                        }
                        last_audio_sent = tokio::time::Instant::now();
                    }
                }
                Some(Ctl::Finish) => {
                    finish_and_drain(sink, stream, &ev_tx, &uplink).await;
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
                                // 上游不再收事件:收摊,别把线程和连接漏着。
                                // 与"上游丢流"路径同形,先给对端一个 Close 再走。
                                let _ = sink.send(Message::Close(None)).await;
                                return;
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
            // 空闲保活。重连后 worker 串行补识期间这条流上零上行帧,厂商的等包超时
            // (45000081)会掐掉会话;配置里的静音相关参数只覆盖"有帧但静音",覆盖
            // 不了"零帧",所以只能我们自己补一帧 100ms 静音。
            // 这条分支随 Ctl::Finish 一起退出循环(finish_and_drain 后直接 return),
            // 所以末包之后不会再有保活帧插到收尾流程里。
            // 代价:静音也计入厂商的流内时钟,此后定稿的 ms 相对"已推真实样本"会前移
            // 一个注入总量(补识窗口最坏 80s → 8 帧 → ≈0.8s),换的是这条流不被掐死
            // (掐死则整段无人识别)。正常录制期采集帧不断,这条分支根本不触发。
            _ = keepalive.tick() => {
                if last_audio_sent.elapsed() >= Duration::from_millis(KEEPALIVE_IDLE_MS) {
                    // 补识窗口内 uplink 必定是空的(worker 压根没 push),不存在
                    // 静音插到半包真实音频前面的乱序问题。
                    let frame = audio_frame(&[0u8; UPLINK_CHUNK_BYTES], false);
                    if let Err(e) = sink.send(Message::binary(frame)).await {
                        let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("保活静音帧发送失败: {e}")) });
                        return;
                    }
                    last_audio_sent = tokio::time::Instant::now();
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
    tail: &[u8],
) {
    // 聚合缓冲里的余音必须先于末包发出:末包一到服务端就收尾,晚发等于截掉句尾。
    if !tail.is_empty() {
        if let Err(e) = sink.send(Message::binary(audio_frame(tail, false))).await {
            let _ = ev_tx.send(CloudEvent::Closed {
                error: Some(format!("尾部音频发送失败: {e}")),
            });
            return;
        }
    }
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
                Err(e) => {
                    return Some(format!("收尾读错误: {e}"));
                }
            }
        }
        None
    })
    .await;

    let _ = sink.send(Message::Close(None)).await;
    // 排干超时/读错时最后一句是否已定稿不可知,带错交给 worker 补识尾部。
    // 对端在末包后的正常 Close/EOF 仍是火山协议的成功结束方式。
    let error = match drained {
        Ok(error) => error,
        Err(_) => Some(format!("收尾超时(>{}ms)", FINAL_DRAIN.as_millis())),
    };
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
        if !should_emit_definite(&d.text) {
            continue;
        }
        if ev_tx.send(CloudEvent::Definite(d)).is_err() {
            return false;
        }
    }
    true
}

/// 空文本定稿只会在 sink 里产生一个空段、还白白推进 last_final_end,过滤掉;
/// 与阿里侧同名函数镜像(刻意各留一份:两家的定稿语义随时可能各自演进)。
fn should_emit_definite(text: &str) -> bool {
    !text.trim().is_empty()
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
    fn adapter_drain_budget_stays_inside_worker_drain_window() {
        // 适配层排干必须严格短于 worker 的窗口,否则尾段定稿在"我们还在收、worker
        // 已放弃"的区间里被静默丢弃(改任一常数时这条会先炸)。
        assert!(
            (FINAL_DRAIN.as_millis() as u64) < CLOUD_DRAIN_MS,
            "FINAL_DRAIN({}ms) 必须 < CLOUD_DRAIN_MS({CLOUD_DRAIN_MS}ms)",
            FINAL_DRAIN.as_millis()
        );
        assert!(FINAL_DRAIN.as_millis() > 0, "排干窗口不能被收窄成 0");
    }

    #[test]
    fn uplink_chunk_is_100ms_of_16k_s16le() {
        assert_eq!(UPLINK_CHUNK_BYTES, 16000 / 10 * 2);
        assert_eq!(QUEUE_CAP_SAMPLES / 16000, 5, "积压闸 = 5s 音频");
    }

    #[test]
    fn keepalive_fires_well_before_vendor_idle_kill() {
        // 火山 sauc:长时间收不到音频包会以 45000081(等包超时)掐掉会话,量级与
        // 阿里的 23s 同类,这里按同一条保守线校核。
        // 实际最坏静默 = KEEPALIVE_IDLE_MS + 一个节拍,必须仍然明显小于它。
        const VENDOR_IDLE_KILL_MS: u64 = 23_000;
        let worst_silence = KEEPALIVE_IDLE_MS + KEEPALIVE_TICK.as_millis() as u64;
        assert!(
            worst_silence < VENDOR_IDLE_KILL_MS,
            "最坏静默 {worst_silence}ms 必须 < 厂商 {VENDOR_IDLE_KILL_MS}ms 掐流线"
        );
        assert!(
            (KEEPALIVE_TICK.as_millis() as u64) < KEEPALIVE_IDLE_MS,
            "节拍要比空闲阈值细,否则保活精度还不如阈值本身"
        );
    }

    #[test]
    fn empty_definite_is_filtered() {
        assert!(!should_emit_definite(""), "空文本定稿只会产生空段");
        assert!(!should_emit_definite("  \n"), "纯空白同理");
        assert!(should_emit_definite("你好"));

        // 走完整 emit 路径:definite=true 但文本为空的分句不产生任何事件。
        let (tx, rx) = crossbeam_channel::unbounded();
        let json = serde_json::json!({
            "result": {"utterances": [{"definite": true, "text": "", "start_time": 0, "end_time": 500}]}
        });
        assert!(emit_response(&json, &tx));
        assert!(
            rx.try_recv().is_err(),
            "空定稿不入下游,免得推进 last_final_end"
        );
    }

    #[test]
    fn flash_prefers_definite_utterances_when_present() {
        let d = DefiniteUtterance {
            text: "你好".into(),
            start_ms: 0,
            end_ms: 700,
            words: vec![],
            lang: String::new(),
        };
        let out = flash_utterances(Some("你好世界".into()), vec![d.clone()], 5000).unwrap();
        assert_eq!(out, vec![d], "有分句就用分句,不被整体文本覆盖");
    }

    #[test]
    fn flash_falls_back_to_whole_text_when_no_definite_flag() {
        let out = flash_utterances(Some("你好".into()), Vec::new(), 5000).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "你好");
        assert_eq!(out[0].end_ms, 5000, "退化条覆盖整段");
    }

    #[test]
    fn flash_empty_text_field_is_genuine_silence() {
        // 字段在、内容空 = 厂商明确说"没人说话",这才允许静默返回空。
        assert!(flash_utterances(Some(String::new()), Vec::new(), 5000)
            .unwrap()
            .is_empty());
        assert!(flash_utterances(Some("  ".into()), Vec::new(), 5000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn flash_missing_text_field_bails_instead_of_reporting_silence() {
        // 字段整个缺失 = 响应形状不符,必须报错,不能折叠成静音把音频丢掉。
        assert!(flash_utterances(None, Vec::new(), 5000).is_err());
    }

    #[test]
    fn head_of_truncates_on_char_boundary() {
        let long: String = "错".repeat(500);
        let h = head_of(&long);
        assert!(h.ends_with('…'));
        assert_eq!(h.chars().count(), 201);
        assert_eq!(head_of("短"), "短", "短体原样返回");
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
