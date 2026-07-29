//! 阿里云 DashScope 语音识别适配:WS 流式(fun-asr-realtime)+ qwen3-asr-flash 批式补识。
//!
//! 分层与火山一致:本文件只管"连接/线程/事件语义",所有 JSON 指令与下行解析都走
//! `protocol.rs` 的纯函数,这里不内联任何协议字面量——协议正确性由 protocol 单测独占负责。
//!
//! 与火山的协议差异(读代码时最容易踩的两点):
//! - 上行是**混合帧**:控制指令走文本帧(JSON),音频走二进制帧(裸 PCM),
//!   不像火山那样所有东西都套自定义二进制帧头。
//! - 建连之后还有一次**握手往返**:必须发 run-task 并等到 `task-started` 才能推音频,
//!   在此之前发二进制帧会被判协议错。因此 open_stream 的同步握手包含这次往返。
//!
//! 同步↔异步的桥:`CloudStream` 的 push/finish 是同步闭包(会话 worker 在自己的
//! crossbeam select 循环里调),而 WS 收发要 async。做法是每条流起一个专用线程跑
//! current_thread runtime,闭包侧只往 `tokio::sync::mpsc` 塞控制消息,不做任何
//! 会阻塞 worker 主循环的事(见 push 处的积压说明)。
//!
//! 关闭语义是与会话 worker 的硬约定(worker 据此决定要不要重连+补识):
//! - `Closed{error: None}` 只在"我方 finish 发出末包之后"出现;
//! - 其它任何终止(服务端主动关、task-failed、IO 错、上游未 finish 就丢流、
//!   厂商自己提前 task-finished)都必须带 `Some(原因)`,否则断连期间的音频会一直
//!   没人识别、直到停录才被发现。

pub mod protocol;

use crate::asr::cloud::{
    f32_to_pcm_s16le, CloudAsr, CloudEvent, CloudStream, DefiniteUtterance, CLOUD_DRAIN_MS,
};
use anyhow::{anyhow, bail, Context};
use futures_util::{SinkExt, StreamExt};
use protocol::{
    batch_request_json, batch_text_from_response, finish_task_msg, parse_event, run_task_msg,
    AliEvent, BATCH_URL, WS_URL,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// 握手(TCP+TLS+WS upgrade+run-task+等 task-started)总上限。比火山多一次服务端
/// 往返,所以给到 6s。超时按"没开起来"处理,返回 Err,由 worker 的退避重连接手——
/// 绝不能挂死在这里,worker 的主循环在等我们返回。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// 末包发出后等服务端吐完收尾定稿(直到 task-finished)的上限。
/// 必须小于 worker 排干窗口,否则 (worker, adapter] 区间的尾段定稿被丢:我们还在
/// 收,worker 已经不听了。故直接由 CLOUD_DRAIN_MS 收窄 500ms 派生,不写死第二个值。
const FINAL_DRAIN: Duration = Duration::from_millis(CLOUD_DRAIN_MS.saturating_sub(500));
/// 待发积压上限(样本数,16k 单声道)。设备回调链上来的一次 push 只有 10–21ms,
/// 按条数计根本量不出时间,所以按样本算:16000×5 = 5s 音频。超过说明连接事实上
/// 已不可用,此时 push 报错让 worker 按断连处理(记缺口 → 重连 → 批式补识)。
const QUEUE_CAP_SAMPLES: usize = 16000 * 5;
/// 上行分包粒度(字节):3200 = 100ms @ 16k/s16le,官方建议的包长。
/// 逐次 push(10–21ms)直发会把帧数放大近 10 倍,徒增服务端调度压力。
const UPLINK_CHUNK_BYTES: usize = 3200;
/// 空闲保活:距上一次**真正发上线**的音频帧超过这么久,就补一帧静音。
///
/// 为什么需要:重连之后会话 worker 会在自己的循环里**同步**跑缺口补识
/// (最多约 20 次 transcribe_batch,40–80s),这段时间它根本回不到 select 循环、
/// 一帧音频都不会 push,新开的流上是**零上行帧**。阿里对此的约束是 run-task 后
/// 23s 内没有音频帧就 task-failed(火山是同类的等包超时 45000081)。
/// run-task 的 `heartbeat:true` 只覆盖"有帧但内容是静音"这种情况,覆盖不了"零帧"。
///
/// 分工(与 session.rs CLOUD_IDLE_CLOSE_MS 注释对称):本保活只负责"worker 被同步
/// 补识阻塞"这类**有界**窗口(≤80s → 注入 ≤0.8s 静音,时钟偏移已在下方分支算过账);
/// **暂停**那种无界静默由 worker 侧的闲置关流兜底——它的门槛(8s)严格小于这里的
/// 10s,所以暂停永远轮不到保活开火。改这两个值时保持这个大小关系(session.rs
/// 有一条常数关系测试盯着)。
pub(crate) const KEEPALIVE_IDLE_MS: u64 = 10_000;
/// 保活检查节拍。粒度要远小于 KEEPALIVE_IDLE_MS,让实际静默上限 ≈ idle + 一个节拍,
/// 与厂商的 23s 之间留足余量。
const KEEPALIVE_TICK: Duration = Duration::from_secs(2);
/// 批式请求超时:调用方按 ≤15s 切段,单请求给足余量但不无限等。
const BATCH_TIMEOUT_S: u64 = 30;

pub struct AliyunAsr {
    api_key: String,
}

impl AliyunAsr {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
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

impl CloudAsr for AliyunAsr {
    /// 同步语义:返回 Ok 即代表"连接已建立、task 已 started",可以立刻推音频。
    /// worker 拿到 Ok 的那一刻就会闭合缺口(session.rs `try_recover`),所以握手
    /// 绝不能异步化成"先给流、失败再报 Closed"——那样握手期间推进来的音频既没送到
    /// 厂商、也不在缺口里,会被静默吞掉。
    fn open_stream(&self) -> anyhow::Result<CloudStream> {
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Ctl>();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<CloudEvent>();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);
        let pending = Arc::new(AtomicUsize::new(0));

        let api_key = self.api_key.clone();
        // 32 位无连字符 hex:官方示例的 task_id 形态,同一 task 的三方(run-task /
        // finish-task / 下行事件)靠它对齐。
        let task_id = uuid::Uuid::new_v4().simple().to_string();
        let thread_pending = pending.clone();
        std::thread::Builder::new()
            .name("aliyun-ws".into())
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
                        handshake(&api_key, &task_id),
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
                    run_session(ws, &task_id, ctl_rx, ev_tx, thread_pending).await;
                });
            })
            .context("起阿里 WS 线程失败")?;

        // 线程内已有 CONNECT_TIMEOUT 闸,这里再多给 1s 余量兜住线程调度抖动;
        // recv_timeout 本身也不会永久阻塞 worker。同样按"短预算"原则:这条 recv
        // 就是 worker 主循环的停顿时长。
        match ready_rx.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bail!("阿里流未能建立: {e}"),
            Err(e) => bail!("阿里流握手无响应: {e}"),
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
                        "阿里待发音频积压 ≥{}s,按断连处理",
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
                    anyhow!("阿里流已关闭,推流失败")
                })
            }),
            finish: Box::new(move || {
                // 只入队,不等排干:worker 是逐源串行调 finish 的,在这里阻塞会把
                // 各源的收尾时延串起来(停录卡顿)。finish-task 的发送与排干由 WS
                // 线程按序完成,并在 FINAL_DRAIN 内必定发出 Closed。
                finish_tx
                    .send(Ctl::Finish)
                    .map_err(|_| anyhow!("阿里流已关闭,末包未发出"))
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
        let body = batch_request_json(&base64_std(&wav));

        let resp = match ureq::post(BATCH_URL)
            .timeout(Duration::from_secs(BATCH_TIMEOUT_S))
            // 批式走标准 HTTP Bearer(流式 WS 那边是小写 bearer,两处都按官方样例写)。
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("content-type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(resp) => resp,
            // 非 2xx:阿里把业务错误说明(鉴权/配额/参数)放在响应体里,只报状态码
            // 等于把唯一有用的信息扔掉。
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                bail!("阿里 qwen3-flash HTTP {code}: {}", head_of(&body));
            }
            Err(e) => return Err(anyhow::Error::new(e).context("阿里 qwen3-flash 请求失败")),
        };

        let text = resp
            .into_string()
            .context("读阿里 qwen3-flash 响应体失败")?;
        let json: serde_json::Value =
            serde_json::from_str(&text).context("解析阿里 qwen3-flash 响应 JSON 失败")?;
        batch_utterances(
            batch_text_from_response(&json),
            &text,
            (samples.len() as u64) * 1000 / 16000,
        )
    }
}

/// 批式响应的"有话/没话/形状不对"三分决策(纯函数,便于单测)。
///
/// 关键区分:`parsed` 为 Err 表示文本路径整个不存在——即响应形状不符(网关拦截页、
/// 错误 JSON、接口改版),必须报错走上层重试/记缺口;`Ok("")` 才是厂商明确表示
/// "这段没人说话"。把前者折叠成后者,等于用一次静默的数据丢失换一次安静的日志。
fn batch_utterances(
    parsed: anyhow::Result<String>,
    body: &str,
    samples_ms: u64,
) -> anyhow::Result<Vec<DefiniteUtterance>> {
    let text = match parsed {
        Ok(t) => t,
        Err(e) => bail!("阿里 qwen3-flash 响应形状不符({e}): {}", head_of(body)),
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    // qwen3-asr-flash 不返回任何时间戳(spec §2.1 已记该偏差),只能退化成一条覆盖
    // [0, 段长] 的 utterance;words 为空 → 下游 diarization 走段级降级,
    // 与本地 Qwen3 路一致。调用方叠加段偏移即可。
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

/// 建连 + run-task + 等 task-started。三步合成"握手"是有意的:没 started 的连接
/// 一个字节音频都不能推,不如在 open_stream 就报错,让 worker 走退避,
/// 而不是先给一条注定要死的流。
async fn handshake(api_key: &str, task_id: &str) -> anyhow::Result<WsStream> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut req = WS_URL.into_client_request().context("构造 WS 握手请求失败")?;
    // 官方样例用小写 "bearer";服务端两种大小写都收,这里与文档保持一致。
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("bearer {api_key}"))
            .context("API Key 含非法头部字符")?,
    );

    let (mut ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .context("WS 建连失败")?;

    ws.send(Message::text(run_task_msg(task_id).to_string()))
        .await
        .context("发送 run-task 失败")?;

    // 等 task-started。注意这里必须真的等到:提前推音频是协议错,服务端会直接
    // task-failed,表现成"偶发一开流就断",极难排查。
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => match parse_event(&t) {
                Ok(AliEvent::Started) => return Ok(ws),
                Ok(AliEvent::Failed { code, msg }) => bail!("run-task 被拒 {code}: {msg}"),
                // started 之前理论上不会有结果/结束事件;真出现也不是"可用连接"。
                Ok(other) => bail!("等 task-started 时收到 {other:?}"),
                // 不认识的帧不致命(厂商可能加新事件类型),继续等。
                Err(e) => eprintln!("阿里握手期下行帧忽略: {e:#}"),
            },
            Some(Ok(Message::Close(f))) => bail!("服务端在 task-started 前关闭连接: {f:?}"),
            // Ping/Pong/Binary:握手期没有业务含义,忽略继续等。
            Some(Ok(_)) => {}
            Some(Err(e)) => bail!("等 task-started 时读错误: {e}"),
            None => bail!("连接在 task-started 前结束"),
        }
    }
}

/// 收发主循环。任何出口都恰好发一次 Closed:None 只属于"我方 finish"这一条路径。
///
/// 关于"run-task 后 23s 内必须发音频帧"的服务端约束:正常录制时采集回调 10–21ms
/// 一次,天然满足;但**重连之后**worker 会先同步跑完缺口补识(最多约 20 次
/// transcribe_batch,40–80s)才回到自己的 select 循环,期间这条新流上零上行帧,
/// 会被厂商按 task-failed 掐掉。故此处有一条 KEEPALIVE_TICK 的定时分支补静音帧。
async fn run_session(
    ws: WsStream,
    task_id: &str,
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
                        // 音频走二进制帧:裸 PCM,无任何封装。
                        let chunk = std::mem::take(&mut uplink);
                        if let Err(e) = sink.send(Message::binary(chunk)).await {
                            let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("音频帧发送失败: {e}")) });
                            return;
                        }
                        last_audio_sent = tokio::time::Instant::now();
                        uplink.reserve(UPLINK_CHUNK_BYTES * 2);
                    }
                }
                Some(Ctl::Finish) => {
                    finish_and_drain(sink, stream, task_id, &ev_tx, &uplink).await;
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
                Some(Ok(Message::Text(t))) => match parse_event(&t) {
                    Ok(ev @ AliEvent::Sentence { .. }) => {
                        if !emit_sentence(ev, &ev_tx) {
                            // 上游不再收事件:收摊,别把线程和连接漏着。
                            let _ = sink.send(Message::Close(None)).await;
                            return;
                        }
                    }
                    Ok(AliEvent::Failed { code, msg }) => {
                        let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("厂商 task-failed {code}: {msg}")) });
                        return;
                    }
                    // 我方还没 finish,厂商却把 task 收了(超时/后台限流):结果不完整,
                    // 必须报 Some 让 worker 记缺口并重连,否则这段音频没人认领。
                    Ok(AliEvent::Finished) => {
                        let _ = ev_tx.send(CloudEvent::Closed { error: Some("厂商提前结束会话(task-finished)".into()) });
                        return;
                    }
                    // 重复的 task-started 之类:无状态可迁,忽略。
                    Ok(AliEvent::Started) => {}
                    // 坏帧不致命(可能是没见过的消息类型),丢弃继续收。
                    Err(e) => eprintln!("阿里下行帧解析失败: {e:#}"),
                },
                // 服务端主动关闭:哪怕是干净的 close 帧,对我们也是"没说完就断了"。
                Some(Ok(Message::Close(f))) => {
                    let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("服务端主动关闭: {f:?}")) });
                    return;
                }
                // Ping/Pong/Binary:tungstenite 自动回 Pong,业务上无内容,忽略。
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
            // 空闲保活。重连后 worker 串行补识期间这条流上零上行帧,厂商 23s 无音频
            // 就 task-failed;run-task 的 heartbeat 参数只覆盖"有帧但静音",覆盖不了
            // "零帧",所以只能我们自己补一帧 100ms 静音。
            // 这条分支随 Ctl::Finish 一起退出循环(finish_and_drain 后直接 return),
            // 所以末包之后不会再有保活帧插到收尾流程里。
            // 代价:静音也计入厂商的流内时钟,此后定稿的 ms 相对"已推真实样本"会前移
            // 一个注入总量(补识窗口最坏 80s → 8 帧 → ≈0.8s),换的是这条流不被掐死
            // (掐死则整段无人识别)。正常录制期采集帧不断,这条分支根本不触发。
            _ = keepalive.tick() => {
                if last_audio_sent.elapsed() >= Duration::from_millis(KEEPALIVE_IDLE_MS) {
                    // 补识窗口内 uplink 必定是空的(worker 压根没 push),不存在
                    // 静音插到半包真实音频前面的乱序问题。
                    if let Err(e) = sink.send(Message::binary(vec![0u8; UPLINK_CHUNK_BYTES])).await {
                        let _ = ev_tx.send(CloudEvent::Closed { error: Some(format!("保活静音帧发送失败: {e}")) });
                        return;
                    }
                    last_audio_sent = tokio::time::Instant::now();
                }
            },
        }
    }
}

/// finish-task + 有界排干。排干整体套一层 timeout:厂商吐完 task-finished 通常随即
/// 关连接,但不能把"它不关"变成我们挂死。
async fn finish_and_drain(
    mut sink: futures_util::stream::SplitSink<WsStream, Message>,
    mut stream: futures_util::stream::SplitStream<WsStream>,
    task_id: &str,
    ev_tx: &crossbeam_channel::Sender<CloudEvent>,
    tail: &[u8],
) {
    // 聚合缓冲里的余音必须先于 finish-task 发出:指令一到服务端就收尾,
    // 晚发等于截掉句尾。
    if !tail.is_empty() {
        if let Err(e) = sink.send(Message::binary(tail.to_vec())).await {
            let _ = ev_tx.send(CloudEvent::Closed {
                error: Some(format!("尾部音频发送失败: {e}")),
            });
            return;
        }
    }
    if let Err(e) = sink
        .send(Message::text(finish_task_msg(task_id).to_string()))
        .await
    {
        let _ = ev_tx.send(CloudEvent::Closed {
            error: Some(format!("finish-task 发送失败: {e}")),
        });
        return;
    }

    let drained = tokio::time::timeout(FINAL_DRAIN, async {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(Message::Text(t)) => match parse_event(&t) {
                    Ok(ev @ AliEvent::Sentence { .. }) => {
                        if !emit_sentence(ev, ev_tx) {
                            return None;
                        }
                    }
                    // 我方 finish 之后的 task-finished 是正常收尾。
                    Ok(AliEvent::Finished) => return None,
                    Ok(AliEvent::Failed { code, msg }) => {
                        return Some(format!("厂商 task-failed {code}: {msg}"));
                    }
                    Ok(AliEvent::Started) => {}
                    Err(e) => eprintln!("阿里收尾帧解析失败: {e:#}"),
                },
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                // finish-task 已发出,读错不改变关闭语义(仍是 None),但别把原因吞了:
                // 收尾阶段的 IO 错是排查"最后一句没出来"的第一现场。
                Err(e) => {
                    eprintln!("阿里收尾读错误: {e}");
                    return None;
                }
            }
        }
        None
    })
    .await;

    let _ = sink.send(Message::Close(None)).await;
    // 末包已发出:超时/对端关闭/读错都只是收尾过程的结束方式,没有重连语义 → None。
    // 只有厂商明确回 task-failed 才带上原因(录制已停,worker 排干阶段不区分,留作日志)。
    let error = drained.unwrap_or(None);
    let _ = ev_tx.send(CloudEvent::Closed { error });
}

/// 把一条 sentence 摊成事件。返回 false 表示上游已不再接收(通道断开)。
///
/// 三条语义分支(顺序不能换):
/// 1. `heartbeat=true` → 保活占位句,直接丢。它常带空文本,若不丢会以 Interim
///    的形式把界面上已有的中间态清成空白;更糟的是万一带 sentence_end,
///    会凭空插一条空定稿进转写。
/// 2. `sentence_end=false` → 中间态,只更新界面用的 Interim(空串没有信息量,不发)。
/// 3. `sentence_end=true` → 定稿,空文本的定稿丢弃(见 `should_emit_definite`),
///    有内容的连词表一起交给下游做 diarization 按词切分。
fn emit_sentence(ev: AliEvent, ev_tx: &crossbeam_channel::Sender<CloudEvent>) -> bool {
    let AliEvent::Sentence {
        text,
        begin_ms,
        end_ms,
        sentence_end,
        words,
        heartbeat,
    } = ev
    else {
        // 非结果事件由收发循环自己迁状态,不该走到这里;真走到也当"无事发生"。
        return true;
    };
    if heartbeat {
        return true;
    }
    if !sentence_end {
        if text.is_empty() {
            return true;
        }
        return ev_tx.send(CloudEvent::Interim { text }).is_ok();
    }
    if !should_emit_definite(&text) {
        return true;
    }
    ev_tx
        .send(CloudEvent::Definite(DefiniteUtterance {
            text,
            start_ms: begin_ms,
            // 定稿帧理论上必带 end_time;真缺了就退化成零长段,保证 end >= start,
            // 下游按 [start, end] 取音频时不会算出负长度。
            end_ms: end_ms.unwrap_or(begin_ms),
            words,
            // 厂商不给语种标签 → 语言过滤走文本兜底(与火山同)。
            lang: String::new(),
        }))
        .is_ok()
}

/// 空文本定稿只会在 sink 里产生一个空段、还白白推进 last_final_end,过滤掉;
/// 与火山侧同名函数镜像(刻意各留一份:两家的定稿语义随时可能各自演进)。
fn should_emit_definite(text: &str) -> bool {
    !text.trim().is_empty()
}

/// f32 → 内存 WAV(16k 单声道 s16le)。批式接口吃带头的容器格式,而流式吃裸 PCM;
/// 采样值转换复用 `f32_to_pcm_s16le`,保证两条路的钳制规则不漂移。
///
/// (与火山那份实现重复是刻意的:两家的批式格式要求随时可能各自演进,
/// 过早抽公共函数会让一家的调整牵动另一家。)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_rejects_empty_samples_without_network() {
        let asr = AliyunAsr::new("key".into());
        // 空音频本地拦截:不发请求(测试机无凭证也无网,能跑通即证明没走网络)。
        assert!(asr.transcribe_batch(&[]).is_err());
    }

    #[test]
    fn batch_missing_text_path_bails_instead_of_reporting_silence() {
        // 文本路径整个缺失 = 响应形状不符,必须报错,不能折叠成静音把音频丢掉。
        let err = batch_utterances(Err(anyhow!("no text")), r#"{"code":"Throttling"}"#, 5000)
            .expect_err("形状不符应报错");
        assert!(
            format!("{err}").contains("Throttling"),
            "报错要带响应体开头,便于定位: {err}"
        );
    }

    #[test]
    fn batch_empty_text_is_genuine_silence() {
        // 字段在、内容空 = 厂商明确说"没人说话",这才允许静默返回空。
        assert!(batch_utterances(Ok(String::new()), "{}", 5000)
            .unwrap()
            .is_empty());
        assert!(batch_utterances(Ok("  ".into()), "{}", 5000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn batch_text_becomes_single_segment_wide_utterance() {
        let out = batch_utterances(Ok("你好".into()), "{}", 5000).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "你好");
        assert_eq!((out[0].start_ms, out[0].end_ms), (0, 5000), "退化条覆盖整段");
        assert!(out[0].words.is_empty(), "qwen3-flash 无时间戳 → 段级降级");
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
        // 阿里 fun-asr-realtime:run-task 之后 23s 内没有音频帧就直接 task-failed。
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
    fn head_of_truncates_on_char_boundary() {
        let long: String = "错".repeat(500);
        let h = head_of(&long);
        assert!(h.ends_with('…'));
        assert_eq!(h.chars().count(), 201);
        assert_eq!(head_of("短"), "短", "短体原样返回");
    }

    #[test]
    fn empty_definite_is_filtered() {
        assert!(!should_emit_definite(""), "空文本定稿只会产生空段");
        assert!(!should_emit_definite("  \n"), "纯空白同理");
        assert!(should_emit_definite("你好"));

        // 走完整 emit 路径:非保活的 sentence_end + 空文本不产生任何事件。
        let (tx, rx) = crossbeam_channel::unbounded();
        assert!(emit_sentence(sentence("", 0, Some(500), true, false), &tx));
        assert!(emit_sentence(
            sentence("  ", 0, Some(500), true, false),
            &tx
        ));
        assert!(
            rx.try_recv().is_err(),
            "空定稿不入下游,免得推进 last_final_end"
        );
    }

    #[test]
    fn wav_bytes_wraps_pcm_with_canonical_44_byte_header() {
        let wav = wav_bytes(&[0.0f32; 100]).unwrap();
        assert_eq!(wav.len(), 44 + 2 * 100, "s16 单声道 WAV = 44 字节头 + 2n");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    /// 测试助手:拼一条 sentence 事件。
    fn sentence(
        text: &str,
        begin_ms: u64,
        end_ms: Option<u64>,
        sentence_end: bool,
        heartbeat: bool,
    ) -> AliEvent {
        AliEvent::Sentence {
            text: text.into(),
            begin_ms,
            end_ms,
            sentence_end,
            words: vec![],
            heartbeat,
        }
    }

    #[test]
    fn heartbeat_sentence_is_discarded() {
        let (tx, rx) = crossbeam_channel::unbounded();
        assert!(emit_sentence(sentence("", 0, None, false, true), &tx));
        // 连 sentence_end=true 的保活句也不能变成定稿。
        assert!(emit_sentence(sentence("x", 0, Some(1), true, true), &tx));
        assert!(rx.try_recv().is_err(), "保活句不产生任何事件");
    }

    #[test]
    fn interim_and_definite_sentences_map_to_events() {
        let (tx, rx) = crossbeam_channel::unbounded();
        assert!(emit_sentence(sentence("你", 0, None, false, false), &tx));
        let CloudEvent::Interim { text } = rx.try_recv().unwrap() else {
            panic!("sentence_end=false 应为 Interim")
        };
        assert_eq!(text, "你");

        let words = vec![crate::asr::cloud::CloudWord {
            text: "你好".into(),
            start_ms: 170,
            end_ms: 800,
        }];
        assert!(emit_sentence(
            AliEvent::Sentence {
                text: "你好".into(),
                begin_ms: 170,
                end_ms: Some(1500),
                sentence_end: true,
                words: words.clone(),
                heartbeat: false,
            },
            &tx
        ));
        let CloudEvent::Definite(u) = rx.try_recv().unwrap() else {
            panic!("sentence_end=true 应为 Definite")
        };
        assert_eq!((u.start_ms, u.end_ms), (170, 1500));
        assert_eq!(u.words, words);
        assert!(u.lang.is_empty(), "厂商不给语种标签");

        // 定稿缺 end_time 时退化成零长段,不产生 end < start。
        assert!(emit_sentence(sentence("尾", 900, None, true, false), &tx));
        let CloudEvent::Definite(u) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!((u.start_ms, u.end_ms), (900, 900));
    }

    #[test]
    fn empty_interim_is_not_emitted() {
        let (tx, rx) = crossbeam_channel::unbounded();
        assert!(emit_sentence(sentence("", 0, None, false, false), &tx));
        assert!(rx.try_recv().is_err(), "空中间态没有信息量,不发");
    }

    #[test]
    fn emit_sentence_reports_dead_receiver() {
        let (tx, rx) = crossbeam_channel::unbounded();
        drop(rx);
        assert!(
            !emit_sentence(sentence("你", 0, None, false, false), &tx),
            "上游不收了要返回 false,让收发循环收摊"
        );
    }
}
